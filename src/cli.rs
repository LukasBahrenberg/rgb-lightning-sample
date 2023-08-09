use crate::bdk_utils::sync_wallet;
use crate::bitcoind_client::BitcoindClient;
use crate::broadcast_tx;
use crate::disk;
use crate::error::Error;
use crate::hex_utils;
use crate::proxy::{get_consignment, post_consignment};
use crate::rgb_utils::get_asset_owned_values;
use crate::rgb_utils::get_rgb_total_amount;
use crate::rgb_utils::RgbUtilities;
use crate::{
	ChannelManager, HTLCStatus, MillisatAmount, NetworkGraph, OnionMessenger, PaymentInfo,
	PaymentInfoStorage, PeerManager,
};
use crate::{FEE_RATE, UTXO_SIZE_SAT};

use amplify::none;
use bdk::bitcoin::hashes::Hash;
use bdk::bitcoin::OutPoint;
use bdk::bitcoin::Txid;
use bdk::database::SqliteDatabase;
use bdk::{FeeRate, SignOptions, Wallet};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::PublicKey;
use bp::seals::txout::CloseMethod;
use bp::seals::txout::ExplicitSeal;
use lightning::chain::keysinterface::{EntropySource, KeysManager};
use lightning::ln::channelmanager::{PaymentId, RecipientOnionFields, Retry};
use lightning::ln::msgs::NetAddress;
use lightning::ln::{PaymentHash, PaymentPreimage};
use lightning::onion_message::{CustomOnionMessageContents, Destination, OnionMessageContents};
use lightning::rgb_utils::drop_rgb_runtime;
use lightning::rgb_utils::get_rgb_runtime;
use lightning::rgb_utils::write_rgb_payment_info_file;
use lightning::rgb_utils::{
	get_rgb_channel_info, write_rgb_channel_info, RgbInfo, RgbUtxo, RgbUtxos,
};
use lightning::routing::gossip::NodeId;
use lightning::routing::router::{PaymentParameters, RouteParameters};
use lightning::util::config::{ChannelHandshakeConfig, ChannelHandshakeLimits, UserConfig};
use lightning::util::ser::{Writeable, Writer};
use lightning_invoice::payment::pay_invoice;
use lightning_invoice::{utils, Currency, Invoice};
use reqwest::Client as RestClient;
use rgb_core::validation::Validity;
use rgbstd::containers::Bindle;
use rgbstd::containers::BuilderSeal;
use rgbstd::containers::Transfer as RgbTransfer;
use rgbstd::contract::{ContractId, GraphSeal, SecretSeal};
use rgbstd::interface::TypedState;
use rgbstd::persistence::Inventory;
use rgbstd::Txid as RgbTxid;
use rgbwallet::RgbTransport;
use seals::txout::blind::BlindSeal;
use seals::txout::TxPtr;
use serde::{Deserialize, Serialize};
use strict_encoding::{FieldName, TypeName};

use rand::Rng;

use std::convert::{TryFrom, TryInto};
use std::env;
use std::fs;
use std::io;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MIN_CREATE_UTXOS_SATS: u64 = 10000;
const UTXO_NUM: u8 = 4;

const OPENCHANNEL_MIN_SAT: u64 = 5506;
const OPENCHANNEL_MAX_SAT: u64 = 16777215;

const DUST_LIMIT_MSAT: u64 = 546000;

const HTLC_MIN_MSAT: u64 = 3000000;

const INVOICE_MIN_MSAT: u64 = HTLC_MIN_MSAT;

#[derive(Serialize, Deserialize)]
struct BlindedInfo {
	contract_id: Option<ContractId>,
	seal: BlindSeal<TxPtr>,
	consumed: bool,
}

pub(crate) struct LdkUserInfo {
	pub(crate) bitcoind_rpc_username: String,
	pub(crate) bitcoind_rpc_password: String,
	pub(crate) bitcoind_rpc_port: u16,
	pub(crate) bitcoind_rpc_host: String,
	pub(crate) ldk_storage_dir_path: String,
	pub(crate) ldk_peer_listening_port: u16,
	pub(crate) ldk_announced_listen_addr: Vec<NetAddress>,
	pub(crate) ldk_announced_node_name: [u8; 32],
	pub(crate) network: Network,
}

struct UserOnionMessageContents {
	tlv_type: u64,
	data: Vec<u8>,
}

impl CustomOnionMessageContents for UserOnionMessageContents {
	fn tlv_type(&self) -> u64 {
		self.tlv_type
	}
}

impl Writeable for UserOnionMessageContents {
	fn write<W: Writer>(&self, w: &mut W) -> Result<(), std::io::Error> {
		w.write_all(&self.data)
	}
}

#[derive(Clone)]
pub(crate) struct SharedData {
	pub(crate) peer_manager: Arc<PeerManager>,
	pub(crate) channel_manager: Arc<ChannelManager>, 
	pub(crate) keys_manager: Arc<KeysManager>,
	pub(crate) network_graph: Arc<NetworkGraph>,
	pub(crate) onion_messenger: Arc<OnionMessenger>,
	pub(crate) inbound_payments: PaymentInfoStorage,
	pub(crate) outbound_payments: PaymentInfoStorage,
	pub(crate) ldk_data_dir: String,
	pub(crate) network: Network,
	pub(crate) logger: Arc<disk::FilesystemLogger>,
	pub(crate) bitcoind_client: Arc<BitcoindClient>,
	pub(crate) proxy_client: Arc<RestClient>,
	pub(crate) proxy_url: String,
	pub(crate) proxy_endpoint: String,
	pub(crate) wallet_arc: Arc<Mutex<Wallet<SqliteDatabase>>>,
	pub(crate) electrum_url: String,
}

impl SharedData {
	pub(crate) fn new(
		peer_manager: Arc<PeerManager>, 
		channel_manager: Arc<ChannelManager>, 
		keys_manager: Arc<KeysManager>, 
		network_graph: Arc<NetworkGraph>, 
		onion_messenger: Arc<OnionMessenger>, 
		inbound_payments: PaymentInfoStorage, 
		outbound_payments: PaymentInfoStorage, 
		ldk_data_dir: String, 
		network: Network, 
		logger: Arc<disk::FilesystemLogger>, 
		bitcoind_client: Arc<BitcoindClient>, 
		proxy_client: Arc<RestClient>, 
		proxy_url: String, 
		proxy_endpoint: String, 
		wallet_arc: Arc<Mutex<Wallet<SqliteDatabase>>>, 
		electrum_url: String,
	) -> Self {
		Self {
			peer_manager,
			channel_manager,
			keys_manager,
			network_graph,
			onion_messenger,
			inbound_payments,
			outbound_payments,
			ldk_data_dir,
			network,
			logger,
			bitcoind_client,
			proxy_client,
			proxy_url,
			proxy_endpoint,
			wallet_arc,
			electrum_url,
		}
	}
}

pub(crate) async fn poll_for_user_input(
	shared_data: SharedData,
) {
	println!(
		"LDK startup successful. Enter \"help\" to view available commands. Press Ctrl-D to quit."
	);
	println!("LDK logs are available at <your-supplied-ldk-data-dir-path>/.ldk/logs");
	println!("Local Node ID is {}", shared_data.channel_manager.get_our_node_id());
	loop {
		print!("> ");
		io::stdout().flush().unwrap(); // Without flushing, the `>` doesn't print
		let mut line = String::new();
		if let Err(e) = io::stdin().read_line(&mut line) {
			break println!("ERROR: {}", e);
		}

		if line.len() == 0 {
			// We hit EOF / Ctrl-D
			break;
		}

		let (response, silent) = handle_command(line, shared_data.clone()).await;
		let response = response.unwrap();

		if !silent {
			println!("{response}");
		}

		if response == "quitting" {
			break;
		}
	}
}

pub(crate) async fn handle_command(
	line: String, 
	shared_data: SharedData,
) -> (Option<String>, bool) {

	// disassemble shared_data
	let peer_manager = shared_data.peer_manager.clone();
	let channel_manager = shared_data.channel_manager.clone();
	let keys_manager = shared_data.keys_manager.clone();
	let network_graph = shared_data.network_graph.clone();
	let onion_messenger = shared_data.onion_messenger.clone();
	let inbound_payments = shared_data.inbound_payments.clone();
	let outbound_payments = shared_data.outbound_payments.clone();
	let ldk_data_dir = shared_data.ldk_data_dir.clone();
	let network = shared_data.network.clone();
	let logger = shared_data.logger.clone();
	let bitcoind_client = shared_data.bitcoind_client.clone();
	let proxy_client = shared_data.proxy_client.clone();
	let proxy_url = &shared_data.proxy_url.clone();
	let proxy_endpoint = &shared_data.proxy_endpoint.clone();
	let wallet_arc = shared_data.wallet_arc.clone();
	let electrum_url = shared_data.electrum_url.clone();

	// handle command
	let mut words = line.split_whitespace();
	if let Some(word) = words.next() {
		match word {
			"help" => { 
				help();
				let response = "help not displayed over api";
				return (Some(response.to_string()), true);
			},
			"mine" => {
				if network != Network::Regtest {
					let response = ("ERROR: mine command is available only on regtest");
					return (Some(response.to_string()), false);
				}
				let num_blocks = words.next();

				if num_blocks.is_none() {
					let response = ("ERROR: mine has 1 required argument: `mine num_blocks`");
					return (Some(response.to_string()), false);
				}

				let num_blocks: Result<u16, _> = num_blocks.unwrap().parse();
				if num_blocks.is_err() {
					let response = ("ERROR: num_blocks must be a number");
					return (Some(response.to_string()), false);
				}

				mine(&bitcoind_client, num_blocks.unwrap()).await;

				let response = "mined blocks";
				return (Some(response.to_string()), true);
			}
			"listunspent" => {
				let wallet = wallet_arc.lock().unwrap();
				let unspents = wallet.list_unspent().expect("unspents");
				let mut response = String::new();
				response.push_str("Unspents:\n");
				println!("Unspents:");
				for unspent in unspents {
					response.push_str(&format!(" - {unspent:?}\n"));
				}
				return (Some(response.to_string()), false)
			}
			"getaddress" => {
				let wallet = wallet_arc.lock().unwrap();
				let address = wallet
					.get_address(bdk::wallet::AddressIndex::New)
					.expect("valid address")
					.address;
				let response = format!("Address: {address}");
				return (Some(response.to_string()), false);
			}
			"createutxos" => {
				let wallet = wallet_arc.lock().unwrap();
				sync_wallet(&wallet, electrum_url.clone());

				let rgb_utxos_path = format!("{}/rgb_utxos", ldk_data_dir.clone());
				let serialized_utxos =
					fs::read_to_string(&rgb_utxos_path).expect("able to read rgb utxos file");
				let mut rgb_utxos: RgbUtxos =
					serde_json::from_str(&serialized_utxos).expect("valid rgb utxos");
				let unspendable_utxos: Vec<OutPoint> =
					rgb_utxos.utxos.iter().map(|u| u.outpoint).collect();

				let unspendable_amt: u64 = wallet
					.list_unspent()
					.expect("unspents")
					.iter()
					.filter(|u| unspendable_utxos.contains(&u.outpoint))
					.map(|u| u.txout.value)
					.sum();
				let available =
					wallet.get_balance().expect("wallet balance").get_total() - unspendable_amt;
				if available < MIN_CREATE_UTXOS_SATS {
					let response = format!(
						"ERROR: not enough funds, call getaddress and send {} satoshis",
						MIN_CREATE_UTXOS_SATS - available
					);
					return (Some(response.to_string()), false);
				}

				let mut tx_builder = wallet.build_tx();
				tx_builder
					.unspendable(unspendable_utxos)
					.fee_rate(FeeRate::from_sat_per_vb(FEE_RATE))
					.ordering(bdk::wallet::tx_builder::TxOrdering::Untouched);
				for _i in 0..UTXO_NUM {
					tx_builder.add_recipient(
						wallet
							.get_address(bdk::wallet::AddressIndex::New)
							.expect("address")
							.script_pubkey(),
						UTXO_SIZE_SAT,
					);
				}
				let (mut psbt, _details) =
					tx_builder.finish().expect("successful psbt creation");

				wallet.sign(&mut psbt, SignOptions::default()).expect("successful sign");

				let tx = psbt.extract_tx();
				broadcast_tx(&tx, electrum_url.clone());

				for i in 0..UTXO_NUM {
					rgb_utxos.utxos.push(RgbUtxo {
						outpoint: OutPoint { txid: tx.txid(), vout: i as u32 },
						colored: false,
					});
				}
				let serialized_utxos =
					serde_json::to_string(&rgb_utxos).expect("valid rgb utxos");
				fs::write(rgb_utxos_path, serialized_utxos)
					.expect("able to write rgb utxos file");

				sync_wallet(&wallet, electrum_url.clone());
				let response = ("UTXO creation complete");
				return (Some(response.to_string()), false);
			}
			"issueasset" => {
				let amount = words.next();
				let ticker = words.next();
				let name = words.next();
				let precision = words.next();

				if amount.is_none() || ticker.is_none() || name.is_none() || precision.is_none()
				{
					let response = ("ERROR: issueasset has 4 required arguments: `issueasset <amount> <ticker> <name> <precision>`");
					return (Some(response.to_string()), false);
				}

				let amount: Result<u64, _> = amount.unwrap().parse();
				if amount.is_err() {
					let response = ("ERROR: amount must be a number");
					return (Some(response.to_string()), false);
				}

				let ticker = ticker.unwrap();
				if !ticker.is_ascii() {
					let response = ("ERROR: ticker must be an ASCII string");
					return (Some(response.to_string()), false);
				}

				let name = name.unwrap();
				if !name.is_ascii() {
					let response = ("ERROR: name must be an ASCII string");
					return (Some(response.to_string()), false);
				}

				let precision: Result<u8, _> = precision.unwrap().parse();
				if precision.is_err() {
					let response = ("ERROR: precision must be a number");
					return (Some(response.to_string()), false);
				}

				match check_uncolored_utxos(&ldk_data_dir).await {
					Ok(_) => {}
					Err(e) => {
						let response = ("{e}");
						return (Some(response.to_string()), false);
					}
				}
				let outpoint = get_utxo(&ldk_data_dir).await.outpoint;
				let rgb_utxos_path = format!("{}/rgb_utxos", ldk_data_dir.clone());
				let serialized_utxos =
					fs::read_to_string(&rgb_utxos_path).expect("able to read rgb utxos file");
				let mut rgb_utxos: RgbUtxos =
					serde_json::from_str(&serialized_utxos).expect("valid rgb utxos");
				rgb_utxos
					.utxos
					.iter_mut()
					.find(|u| u.outpoint == outpoint)
					.expect("UTXO found")
					.colored = true;
				let serialized_utxos =
					serde_json::to_string(&rgb_utxos).expect("valid rgb utxos");
				fs::write(rgb_utxos_path, serialized_utxos)
					.expect("able to write rgb utxos file");

				let mut runtime = get_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));
				let contract_id = runtime.issue_contract(
					amount.unwrap(),
					outpoint,
					ticker.to_string(),
					name.to_string(),
					precision.unwrap(),
				);
				drop(runtime);
				drop_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));
				let response = format!("Asset ID: {contract_id}");
				return (Some(response.to_string()), false);
			}
			"assetbalance" => {
				let assetbalance_cmd = "`assetbalance <contract_id>`";
				let contract_id = words.next();

				if contract_id.is_none() {
					let response = format!(
						"ERROR: assetbalance has 1 required argument: `{assetbalance_cmd}`"
					);
					return (Some(response.to_string()), false);
				}

				let contract_id = ContractId::from_str(contract_id.unwrap());
				if contract_id.is_err() {
					let response = ("ERROR: contract_id must be a valid RGB asset ID");
					return (Some(response.to_string()), false);
				}

				let runtime = get_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));
				let total_rgb_amount = match get_rgb_total_amount(
					contract_id.unwrap(),
					&runtime,
					wallet_arc.clone(),
					electrum_url.clone(),
				) {
					Ok(a) => a,
					Err(e) => {
						let response = ("{e}");
						return (Some(response.to_string()), false);
					}
				};

				drop(runtime);
				drop_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));

				let response = format!("Asset balance: {total_rgb_amount}");
				return (Some(response.to_string()), false);
			}
			"sendasset" => {
				let sendasset_cmd = "`sendasset <contract_id> <amt_rgb> <blinded_utxo>`";
				let contract_id = words.next();
				let amt_rgb_str = words.next();
				let blinded_utxo = words.next();

				if contract_id.is_none() || amt_rgb_str.is_none() || blinded_utxo.is_none() {
					let response = format!("ERROR: sendasset has 3 required arguments: `{sendasset_cmd}`");
					return (Some(response.to_string()), false);
				}

				let contract_id = ContractId::from_str(contract_id.unwrap());
				if contract_id.is_err() {
					let response = ("ERROR: contract_id must be a valid RGB asset ID");
					return (Some(response.to_string()), false);
				}
				let contract_id = contract_id.unwrap();

				let amt_rgb: u64 = match amt_rgb_str.unwrap().parse() {
					Ok(amt) => amt,
					Err(e) => {
						let response = ("ERROR: couldn't parse amt_rgb: {e}");
						return (Some(response.to_string()), false);
					}
				};

				let mut runtime = get_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));

				let total_rgb_amount = match get_rgb_total_amount(
					contract_id,
					&runtime,
					wallet_arc.clone(),
					electrum_url.clone(),
				) {
					Ok(a) => a,
					Err(e) => {
						let response = ("{e}");
						return (Some(response.to_string()), false);
					}
				};

				if amt_rgb > total_rgb_amount {
					let response = ("ERROR: do not have enough RGB assets");
					return (Some(response.to_string()), false);
				}

				let blinded_utxo = blinded_utxo.unwrap();
				let concealed_seal = SecretSeal::from_str(blinded_utxo);
				if concealed_seal.is_err() {
					let response = ("ERROR: blinded_utxo must be a valid RGB blinded UTXO");
					return (Some(response.to_string()), false);
				}
				let concealed_seal = concealed_seal.unwrap();

				let asset_owned_values = get_asset_owned_values(
					contract_id,
					&runtime,
					wallet_arc.clone(),
					electrum_url.clone(),
				)
				.expect("known contract");

				let mut asset_transition_builder = runtime
					.transition_builder(
						contract_id,
						TypeName::try_from("RGB20").unwrap(),
						None::<&str>,
					)
					.expect("ok");
				let assignment_id = asset_transition_builder
					.assignments_type(&FieldName::from("beneficiary"))
					.expect("valid assignment");
				let mut beneficiaries = vec![];

				let mut rgb_inputs = vec![];
				let mut input_amount: u64 = 0;
				for (_opout, (outpoint, amount)) in asset_owned_values {
					if input_amount >= amt_rgb {
						break;
					}
					rgb_inputs.push(OutPoint {
						txid: Txid::from_str(&outpoint.txid.to_string()).unwrap(),
						vout: outpoint.vout.into_u32(),
					});
					input_amount += amount;
				}

				let rgb_change_amount = input_amount - amt_rgb;
				if rgb_change_amount > 0 {
					match check_uncolored_utxos(&ldk_data_dir).await {
						Ok(_) => {}
						Err(e) => {
							let response = ("{e}");
							return (Some(response.to_string()), false);
						}
					}
					let rgb_change_outpoint = get_utxo(&ldk_data_dir).await.outpoint;
					let rgb_utxos_path = format!("{}/rgb_utxos", ldk_data_dir.clone());
					let serialized_utxos = fs::read_to_string(&rgb_utxos_path)
						.expect("able to read rgb utxos file");
					let mut rgb_utxos: RgbUtxos =
						serde_json::from_str(&serialized_utxos).expect("valid rgb utxos");
					rgb_utxos
						.utxos
						.iter_mut()
						.find(|u| u.outpoint == rgb_change_outpoint)
						.expect("UTXO found")
						.colored = true;
					let serialized_utxos =
						serde_json::to_string(&rgb_utxos).expect("valid rgb utxos");
					fs::write(rgb_utxos_path, serialized_utxos)
						.expect("able to write rgb utxos file");
					let seal = ExplicitSeal::with(
						CloseMethod::OpretFirst,
						RgbTxid::from_str(&rgb_change_outpoint.txid.to_string())
							.unwrap()
							.into(),
						rgb_change_outpoint.vout,
					);
					let seal = GraphSeal::from(seal);
					let change = TypedState::Amount(rgb_change_amount);
					asset_transition_builder = asset_transition_builder
						.add_raw_state(assignment_id, seal, change)
						.unwrap();
				}

				let wallet = wallet_arc.lock().unwrap();

				let mut builder = wallet.build_tx();
				let address = wallet
					.get_address(bdk::wallet::AddressIndex::New)
					.expect("valid address")
					.address;
				builder
					.add_utxos(&rgb_inputs)
					.expect("valid utxos")
					.add_data(&[1])
					.fee_rate(FeeRate::from_sat_per_vb(FEE_RATE))
					.manually_selected_only()
					.drain_to(address.script_pubkey());
				let psbt = builder.finish().expect("valid psbt finish").0;

				asset_transition_builder = asset_transition_builder
					.add_raw_state(assignment_id, concealed_seal, TypedState::Amount(amt_rgb))
					.expect("ok");
				beneficiaries.push(BuilderSeal::Concealed(concealed_seal));

				let (mut psbt, consignment) = runtime.send_rgb(
					contract_id,
					psbt,
					asset_transition_builder,
					beneficiaries,
				);

				let consignment_path = format!("{}/consignment", ldk_data_dir.clone());
				consignment.save(&consignment_path).expect("successful save");

				let proxy_ref = (*proxy_client).clone();
				let res = post_consignment(
					proxy_ref,
					proxy_url,
					blinded_utxo.to_string(),
					consignment_path.into(),
				)
				.await;
				if res.is_err() || res.unwrap().result.is_none() {
					let response = ("ERROR: unable to post consignment");
					return (Some(response.to_string()), false);
				}

				wallet.sign(&mut psbt, SignOptions::default()).expect("able to sign");
				let tx = psbt.extract_tx();
				broadcast_tx(&tx, electrum_url.clone());

				let transfer =
					consignment.unbindle().validate(runtime.resolver()).unwrap_or_else(|c| c);
				let _status = runtime.accept_transfer(transfer, true).expect("valid transfer");
				drop(runtime);
				drop_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));

				sync_wallet(&wallet, electrum_url.clone());
				
				let response = format!("RGB send complete, txid: {}", tx.txid());
				return (Some(response.to_string()), false);
			}
			"receiveasset" => {
				match check_uncolored_utxos(&ldk_data_dir).await {
					Ok(_) => {}
					Err(e) => {
						let response = ("{e}");
						return (Some(response.to_string()), false);
					}
				}
				let outpoint = get_utxo(&ldk_data_dir).await.outpoint;
				let rgb_utxos_path = format!("{}/rgb_utxos", ldk_data_dir.clone());
				let serialized_utxos =
					fs::read_to_string(&rgb_utxos_path).expect("able to read rgb utxos file");
				let mut rgb_utxos: RgbUtxos =
					serde_json::from_str(&serialized_utxos).expect("valid rgb utxos");
				rgb_utxos
					.utxos
					.iter_mut()
					.find(|u| u.outpoint == outpoint)
					.expect("UTXO found")
					.colored = true;
				let serialized_utxos =
					serde_json::to_string(&rgb_utxos).expect("valid rgb utxos");
				fs::write(rgb_utxos_path, serialized_utxos)
					.expect("able to write rgb utxos file");

				let seal = ExplicitSeal::with(
					CloseMethod::OpretFirst,
					RgbTxid::from_str(&outpoint.txid.to_string()).unwrap().into(),
					outpoint.vout,
				);
				let seal = GraphSeal::from(seal);

				let mut runtime = get_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));
				runtime.store_seal_secret(seal).expect("successful seal store");
				drop(runtime);
				drop_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));

				let concealed_seal = seal.to_concealed_seal();
				let blinded_utxo = concealed_seal.to_string();

				let blinded_dir = PathBuf::from_str(&ldk_data_dir.clone())
					.expect("valid data dir")
					.join("blinded_utxos");
				let blinded_path = blinded_dir.join(&blinded_utxo);
				let blinded_info = BlindedInfo { contract_id: None, seal, consumed: false };
				let serialized_info =
					serde_json::to_string(&blinded_info).expect("valid rgb info");
				fs::write(blinded_path, serialized_info).expect("successful file write");

				let response = format!("Blinded UTXO: {blinded_utxo}");
				return (Some(response.to_string()), false);
			}
			"refresh" => {
				let blinded_dir = PathBuf::from_str(&ldk_data_dir)
					.expect("valid data dir")
					.join("blinded_utxos");
				let blinded_files = fs::read_dir(blinded_dir).expect("successfult dir read");

				for bf in blinded_files {
					let serialized_info = fs::read_to_string(bf.as_ref().unwrap().path())
						.expect("valid blinded info file");
					let blinded_info: BlindedInfo =
						serde_json::from_str(&serialized_info).expect("valid blinded data");
					if blinded_info.consumed {
						let response = "";
						return (Some(response.to_string()), false);
					}

					let blinded_utxo = blinded_info.seal.to_concealed_seal().to_string();

					let proxy_ref = (*proxy_client).clone();
					let res = get_consignment(proxy_ref, proxy_url, blinded_utxo.clone()).await;
					if res.is_err() || res.as_ref().unwrap().result.is_none() {
						let response = ("WARNING: unable to get consignment");
						return (Some(response.to_string()), false);
					}
					let consignment = res.unwrap().result.unwrap();
					let consignment_bytes =
						base64::decode(consignment).expect("valid consignment");
					let consignment_path =
						format!("{}/consignment_{}", ldk_data_dir.clone(), blinded_utxo);
					fs::write(consignment_path.clone(), consignment_bytes)
						.expect("unable to write file");
					let consignment = Bindle::<RgbTransfer>::load(consignment_path)
						.expect("successful consignment load");
					let transfer: RgbTransfer = consignment.clone().unbindle();

					let mut runtime = get_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));

					let mut minimal_contract = transfer.clone().into_contract();
					minimal_contract.bundles = none!();
					minimal_contract.terminals = none!();
					let minimal_contract_validated =
						match minimal_contract.clone().validate(runtime.resolver()) {
							Ok(consignment) => consignment,
							Err(consignment) => consignment,
						};
					runtime
						.import_contract(minimal_contract_validated)
						.expect("failure importing issued contract");

					let validated_transfer =
						transfer.validate(runtime.resolver()).expect("invalid contract");
					let status = runtime
						.accept_transfer(validated_transfer, true)
						.expect("valid transfer");
					drop(runtime);
					drop_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));
					let validity = status.validity();
					if !matches!(validity, Validity::Valid) {
						let response = ("WARNING: error accepting transfer");
						return (Some(response.to_string()), false);
					}

					let wallet = wallet_arc.lock().unwrap();
					sync_wallet(&wallet, electrum_url.clone());

					fs::remove_file(bf.unwrap().path()).expect("successful file remove");
				}

				let response = ("Refresh complete");
				return (Some(response.to_string()), false);
			}
			"openchannel" => {
				let peer_pubkey_and_ip_addr = words.next();
				let channel_value_sat = words.next();
				let push_value_msat = words.next();
				let contract_id = words.next();
				let channel_value_rgb = words.next();
				if peer_pubkey_and_ip_addr.is_none()
					|| channel_value_sat.is_none()
					|| push_value_msat.is_none()
					|| contract_id.is_none() || channel_value_rgb.is_none()
				{
					let response = ("ERROR: openchannel has 5 required arguments: `openchannel pubkey@host:port chan_amt_satoshis push_amt_msatoshis rgb_contract_id chan_amt_rgb` [--public]");
					return (Some(response.to_string()), false);
				}
				let peer_pubkey_and_ip_addr = peer_pubkey_and_ip_addr.unwrap();
				let (pubkey, peer_addr) =
					match parse_peer_info(peer_pubkey_and_ip_addr.to_string()) {
						Ok(info) => info,
						Err(e) => {
							let response = format!("{:?}", e.into_inner().unwrap());
							return (Some(response.to_string()), false);
						}
					};

				let chan_amt_sat: Result<u64, _> = channel_value_sat.unwrap().parse();
				if chan_amt_sat.is_err() {
					let response = ("ERROR: channel amount must be a number");
					return (Some(response.to_string()), false);
				}
				let chan_amt_sat = chan_amt_sat.unwrap();
				if chan_amt_sat < OPENCHANNEL_MIN_SAT {
					let response = format!(
						"ERROR: channel amount must be equal or higher than {}",
						OPENCHANNEL_MIN_SAT
					);
					return (Some(response.to_string()), false);
				}
				if chan_amt_sat > OPENCHANNEL_MAX_SAT {
					let response = format!(
						"ERROR: channel amount must be equal or less than {}",
						OPENCHANNEL_MAX_SAT
					);
					return (Some(response.to_string()), false);
				}

				let push_amt_msat: Result<u64, _> = push_value_msat.unwrap().parse();
				if push_amt_msat.is_err() {
					let response = ("ERROR: push amount must be a number");
					return (Some(response.to_string()), false);
				}
				let push_amt_msat = push_amt_msat.unwrap();
				if push_amt_msat < DUST_LIMIT_MSAT {
					let response = format!(
						"ERROR: push amount must be equal or higher than the dust limit ({})",
						DUST_LIMIT_MSAT
					);
					return (Some(response.to_string()), false);
				}

				let contract_id = ContractId::from_str(contract_id.unwrap());
				if contract_id.is_err() {
					let response = ("ERROR: contract_id must be a valid RGB asset ID");
					return (Some(response.to_string()), false);
				}
				let contract_id = contract_id.unwrap();

				let chan_amt_rgb: Result<u64, _> = channel_value_rgb.unwrap().parse();
				if chan_amt_rgb.is_err() {
					let response = ("ERROR: channel RGB amount must be a number");
					return (Some(response.to_string()), false);
				}
				let chan_amt_rgb = chan_amt_rgb.unwrap();

				let runtime = get_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));

				let total_rgb_amount = match get_rgb_total_amount(
					contract_id,
					&runtime,
					wallet_arc.clone(),
					electrum_url.clone(),
				) {
					Ok(a) => a,
					Err(e) => {
						let response = ("{e}");
						return (Some(response.to_string()), false);
					}
				};
				drop(runtime);
				drop_rgb_runtime(&PathBuf::from(ldk_data_dir.clone()));

				if chan_amt_rgb > total_rgb_amount {
					let response = ("ERROR: do not have enough RGB assets");
					return (Some(response.to_string()), false);
				}

				if connect_peer_if_necessary(pubkey, peer_addr, peer_manager.clone())
					.await
					.is_err()
				{
					let response = ("");
					return (Some(response.to_string()), false);
				};

				let announce_channel = match words.next() {
					Some("--public") | Some("--public=true") => true,
					Some("--public=false") => false,
					Some(_) => {
						let response = ("ERROR: invalid `--public` command format. Valid formats: `--public`, `--public=true` `--public=false`");
						return (Some(response.to_string()), false);
					}
					None => false,
				};

				let open_channel_result = open_channel(
					pubkey,
					chan_amt_sat,
					push_amt_msat,
					announce_channel,
					channel_manager.clone(),
					proxy_endpoint,
				);
				if open_channel_result.is_err() {
					let response = format!("ERROR: {:?}", open_channel_result.err().unwrap());
					return (Some(response.to_string()), false);
				}

				let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
				let _ = disk::persist_channel_peer(
					Path::new(&peer_data_path),
					peer_pubkey_and_ip_addr,
				);

				let temporary_channel_id = open_channel_result.unwrap();
				let channel_rgb_info_path =
					format!("{}/{}", ldk_data_dir.clone(), hex::encode(temporary_channel_id));
				let rgb_info = RgbInfo {
					contract_id,
					local_rgb_amount: chan_amt_rgb,
					remote_rgb_amount: 0,
				};
				write_rgb_channel_info(&PathBuf::from(&channel_rgb_info_path), &rgb_info);
				let response = "Channel opened";
				return (Some(response.to_string()), true);
			}
			"sendpayment" => {
				let invoice_str = words.next();
				if invoice_str.is_none() {
					let response = ("ERROR: sendpayment requires an invoice: `sendpayment <invoice>`");
					return (Some(response.to_string()), false);
				}

				let invoice = match Invoice::from_str(invoice_str.unwrap()) {
					Ok(inv) => inv,
					Err(e) => {
						let response = format!("ERROR: invalid invoice: {:?}", e);
						return (Some(response.to_string()), false);
					}
				};

				if let Some(amt_msat) = invoice.amount_milli_satoshis() {
					if amt_msat < INVOICE_MIN_MSAT {
						let response = ("ERROR: msat amount in invoice cannot be less than {INVOICE_MIN_MSAT}");
						return (Some(response.to_string()), false);
					}
				} else {
					let response = ("ERROR: msat amount missing in invoice");
					return (Some(response.to_string()), false);
				}

				send_payment(
					&*channel_manager,
					&invoice,
					outbound_payments.clone(),
					PathBuf::from(&ldk_data_dir),
				);

				let response = "Payment initiated";
				return (Some(response.to_string()), true);
			}
			"keysend" => {
				let keysend_cmd = "`keysend <dest_pubkey> <amt_msat> <contract_id> <amt_rgb>`";
				let dest_pubkey = match words.next() {
					Some(dest) => match hex_utils::to_compressed_pubkey(dest) {
						Some(pk) => pk,
						None => {
							let response = ("ERROR: couldn't parse destination pubkey");
							return (Some(response.to_string()), false);
						}
					},
					None => {
						let response = format!("ERROR: keysend requires a destination pubkey: {keysend_cmd}");
						return (Some(response.to_string()), false);
					}
				};
				let amt_msat_str =
					match words.next() {
						Some(amt) => amt,
						None => {
							let response = format!("ERROR: keysend requires an amount in millisatoshis: {keysend_cmd}");
							return (Some(response.to_string()), false);
						}
					};
				let amt_msat: u64 = match amt_msat_str.parse() {
					Ok(amt) => amt,
					Err(e) => {
						let response = format!("ERROR: couldn't parse amount_msat: {}", e);
						return (Some(response.to_string()), false);
					}
				};
				if amt_msat < HTLC_MIN_MSAT {
					let response = format!("ERROR: amount_msat cannot be less than {HTLC_MIN_MSAT}");
					return (Some(response.to_string()), false);
				}
				let contract_id = match words.next() {
					Some(contract_id_str) => match ContractId::from_str(contract_id_str) {
						Ok(cid) => cid,
						Err(_) => {
							let response = format!("ERROR: invalid contract ID: {contract_id_str}");
							return (Some(response.to_string()), false);
						}
					},
					None => {
						let response = format!("ERROR: keysend requires a contract ID: {keysend_cmd}");
						return (Some(response.to_string()), false);
					}
				};
				let amt_rgb_str = match words.next() {
					Some(amt) => amt,
					None => {
						let response = format!("ERROR: keysend requires an RGB amount: {keysend_cmd}");
						return (Some(response.to_string()), false);
					}
				};
				let amt_rgb: u64 = match amt_rgb_str.parse() {
					Ok(amt) => amt,
					Err(e) => {
						let response = format!("ERROR: couldn't parse amt_rgb: {e}");
						return (Some(response.to_string()), false);
					}
				};
				keysend(
					&*channel_manager,
					dest_pubkey,
					amt_msat,
					&*keys_manager,
					outbound_payments.clone(),
					contract_id,
					amt_rgb,
					PathBuf::from(&ldk_data_dir),
				);

				let response = "Keysend initiated";
				return (Some(response.to_string()), true);
			}
			"getinvoice" => {
				let getinvoice_cmd =
					"`getinvoice <amt_msats> <expiry_secs> <rgb_contract_id> <amt_rgb>`";
				let amt_str = words.next();
				let expiry_secs_str = words.next();
				let contract_id_str = words.next();
				let amt_rgb_str = words.next();

				if amt_str.is_none()
					|| expiry_secs_str.is_none()
					|| contract_id_str.is_none()
					|| amt_rgb_str.is_none()
				{
					let response = format!("ERROR: getinvoice has 4 required arguments: {getinvoice_cmd}");
					return (Some(response.to_string()), false);
				}

				let amt_msat: Result<u64, _> = amt_str.unwrap().parse();
				if amt_msat.is_err() {
					let response = ("ERROR: getinvoice provided payment amount was not a number");
					return (Some(response.to_string()), false);
				}
				let amt_msat = amt_msat.unwrap();
				if amt_msat < INVOICE_MIN_MSAT {
					let response = format!("ERROR: amt_msat cannot be less than {INVOICE_MIN_MSAT}");
					return (Some(response.to_string()), false);
				}

				let expiry_secs: Result<u32, _> = expiry_secs_str.unwrap().parse();
				if expiry_secs.is_err() {
					let response = ("ERROR: getinvoice provided expiry was not a number");
					return (Some(response.to_string()), false);
				}

				let contract_id_str = contract_id_str.unwrap();
				let contract_id = match ContractId::from_str(contract_id_str) {
					Ok(cid) => cid,
					Err(_) => {
						let response = format!("ERROR: invalid contract ID: {contract_id_str}");
						return (Some(response.to_string()), false);
					}
				};

				let amt_rgb: u64 = match amt_rgb_str.unwrap().parse() {
					Ok(amt) => amt,
					Err(e) => {
						let response = format!("ERROR: couldn't parse amt_rgb: {e}");
						return (Some(response.to_string()), false);
					}
				};

				let response = get_invoice(
					amt_msat,
					Arc::clone(&inbound_payments),
					&*channel_manager,
					Arc::clone(&keys_manager),
					network,
					expiry_secs.unwrap(),
					Arc::clone(&logger),
					contract_id,
					amt_rgb,
				).unwrap();

				return (Some(response.to_string()), true);
			}
			"generatetestpreimage" => {
				// create random preimage for testing
				
				let mut rng = rand::thread_rng();
				let payment_preimage: [u8; 32] = rng.gen();
				
				let payment_preimage = PaymentPreimage(payment_preimage);
				let payment_hash = PaymentHash(Sha256::hash(&payment_preimage.0).into_inner());

				let test_preimage_data = TestPreimageData {
					payment_preimage,
					payment_hash,
				};

				let response = format!("This is a preimage / payment hash pair for testing: \n{:?} \nPreimage: {} \nPayment hash: {}", 
					test_preimage_data,
					hex::encode(&test_preimage_data.payment_preimage.0),
					hex::encode(&test_preimage_data.payment_hash.0),
				);
				
				return (Some(response.to_string()), false)
			}
			"gethodlinvoice" => {
				let gethodlinvoice_cmd =
					"`gethodlinvoice <amt_msats> <expiry_secs> <rgb_contract_id> <amt_rgb> <payment_hash>`";
				let amt_str = words.next();
				let expiry_secs_str = words.next();
				let contract_id_str = words.next();
				let amt_rgb_str = words.next();
				let payment_hash = words.next();

				if amt_str.is_none()
					|| expiry_secs_str.is_none()
					|| contract_id_str.is_none()
					|| amt_rgb_str.is_none()
					|| payment_hash.is_none()
				{
					let response = format!("ERROR: getinvoice has 5 required arguments: {gethodlinvoice_cmd}");
					
					return (Some(response.to_string()), false)
				}

				let amt_msat: Result<u64, _> = amt_str.unwrap().parse();
				if amt_msat.is_err() {
					let response = format!("ERROR: getinvoice provided payment amount was not a number");
					
					return (Some(response.to_string()), false)
				}
				let amt_msat = amt_msat.unwrap();
				if amt_msat < INVOICE_MIN_MSAT {
					let response = format!("ERROR: amt_msat cannot be less than {INVOICE_MIN_MSAT}");
					
					return (Some(response.to_string()), false)
				}

				let expiry_secs: Result<u32, _> = expiry_secs_str.unwrap().parse();
				if expiry_secs.is_err() {
					let response = format!("ERROR: getinvoice provided expiry was not a number");
					
					return (Some(response.to_string()), false)
				}

				let contract_id_str = contract_id_str.unwrap();
				let contract_id = match ContractId::from_str(contract_id_str) {
					Ok(cid) => cid,
					Err(_) => {
						let response = format!("ERROR: invalid contract ID: {contract_id_str}");
						
						return (Some(response.to_string()), false)
					}
				};

				let amt_rgb: u64 = match amt_rgb_str.unwrap().parse() {
					Ok(amt) => amt,
					Err(e) => {
						let response = format!("ERROR: couldn't parse amt_rgb: {e}");
		
						return (Some(response.to_string()), false)
					}
				};

				let payment_hash: String = match payment_hash.unwrap().parse::<String>() {
					Ok(hash) => hash,
					Err(e) => {
						let response = format!("ERROR: couldn't parse payment_hash: {e}");
		
						return (Some(response.to_string()), false)
					}
				};

				let response = get_hodl_invoice(
					amt_msat,
					Arc::clone(&inbound_payments),
					&*channel_manager,
					Arc::clone(&keys_manager),
					network,
					expiry_secs.unwrap(),
					Arc::clone(&logger),
					contract_id,
					amt_rgb,
					payment_hash,
				);
				
				return (Some(response.to_string()), false)
			}
			"settlehodlinvoice" => {
				let getinvoice_cmd =
					"`settlehodlinvoice <preimage>`";
				let preimage = words.next();

				if preimage.is_none()
				{
					let response = format!("ERROR: getinvoice has 1 required arguments: {getinvoice_cmd}");
					
					return (Some(response.to_string()), false)

				}

				let preimage: String = match preimage.unwrap().parse::<String>() {
					Ok(preimage) => preimage,
					Err(e) => {
						let response = format!("ERROR: couldn't parse preimage: {e}");
		
						return (Some(response.to_string()), false)
					}
				};

				let response = settle_hodl_invoice(
					preimage,
					&*channel_manager,
				);
				
				return (Some(response.to_string()), false)
			}
			"connectpeer" => {
				let peer_pubkey_and_ip_addr = words.next();
				if peer_pubkey_and_ip_addr.is_none() {
					let response = ("ERROR: connectpeer requires peer connection info: `connectpeer pubkey@host:port`");
					return (Some(response.to_string()), false);
				}
				let (pubkey, peer_addr) =
					match parse_peer_info(peer_pubkey_and_ip_addr.unwrap().to_string()) {
						Ok(info) => info,
						Err(e) => {
							let response = format!("{:?}", e.into_inner().unwrap());
							return (Some(response.to_string()), false);
						}
					};
				if connect_peer_if_necessary(pubkey, peer_addr, peer_manager.clone())
					.await
					.is_ok()
				{
					let response = format!("SUCCESS: connected to peer {}", pubkey);
					return (Some(response.to_string()), false);
				}

				let response = format!("ERROR: unable to connect to peer or already connected {}", pubkey);
				return (Some(response.to_string()), true);
			}
			"disconnectpeer" => {
				let peer_pubkey = words.next();
				if peer_pubkey.is_none() {
					let response = ("ERROR: disconnectpeer requires peer public key: `disconnectpeer <peer_pubkey>`");
					return (Some(response.to_string()), false);
				}

				let peer_pubkey =
					match bitcoin::secp256k1::PublicKey::from_str(peer_pubkey.unwrap()) {
						Ok(pubkey) => pubkey,
						Err(e) => {
							let response = format!("ERROR: {}", e.to_string());
							return (Some(response.to_string()), false);
						}
					};

				if do_disconnect_peer(
					peer_pubkey,
					peer_manager.clone(),
					channel_manager.clone(),
				)
				.is_ok()
				{
					let response = format!("SUCCESS: disconnected from peer {}", peer_pubkey);
					return (Some(response.to_string()), false);
				}
				let response = format!("ERROR: unable to disconnect from peer {}", peer_pubkey);
				return (Some(response.to_string()), true);
			}
			"listchannels" => {
				let response = list_channels(&channel_manager, &network_graph, ldk_data_dir.clone());
				return (Some(response.to_string()), false);
			}
			"listpayments" => {
				let response = list_payments(inbound_payments.clone(), outbound_payments.clone());
				return (Some(response.to_string()), false);
			}
			"invoicestatus" => {
				let invoice = words.next();
				if invoice.is_none() {
					let response = (
						"ERROR: invoicestatus requires an invoice: `invoicestatus <invoice>`"
					);
					return (Some(response.to_string()), false);
				};
				let invoice = match Invoice::from_str(invoice.unwrap()) {
					Err(e) => {
						let response = format!("ERROR: invalid invoice: {:?}", e);
						return (Some(response.to_string()), false);
					}
					Ok(v) => v,
				};

				let response = invoice_status(inbound_payments.clone(), invoice).unwrap();
				return (Some(response.to_string()), false);
			}
			"closechannel" => {
				let channel_id_str = words.next();
				if channel_id_str.is_none() {
					let response = ("ERROR: closechannel requires a channel ID: `closechannel <channel_id> <peer_pubkey>`");
					return (Some(response.to_string()), false);
				}
				let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
				if channel_id_vec.is_none() || channel_id_vec.as_ref().unwrap().len() != 32 {
					let response = ("ERROR: couldn't parse channel_id");
					return (Some(response.to_string()), false);
				}
				let mut channel_id = [0; 32];
				channel_id.copy_from_slice(&channel_id_vec.unwrap());

				let peer_pubkey_str = words.next();
				if peer_pubkey_str.is_none() {
					let response = ("ERROR: closechannel requires a peer pubkey: `closechannel <channel_id> <peer_pubkey>`");
					return (Some(response.to_string()), false);
				}
				let peer_pubkey_vec = match hex_utils::to_vec(peer_pubkey_str.unwrap()) {
					Some(peer_pubkey_vec) => peer_pubkey_vec,
					None => {
						let response = ("ERROR: couldn't parse peer_pubkey");
						return (Some(response.to_string()), false);
					}
				};
				let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
					Ok(peer_pubkey) => peer_pubkey,
					Err(_) => {
						let response = ("ERROR: couldn't parse peer_pubkey");
						return (Some(response.to_string()), false);
					}
				};

				let response = close_channel(channel_id, peer_pubkey, channel_manager.clone()).unwrap();
				return (Some(response.to_string()), false);
			}
			"forceclosechannel" => {
				let channel_id_str = words.next();
				if channel_id_str.is_none() {
					let response = ("ERROR: forceclosechannel requires a channel ID: `forceclosechannel <channel_id> <peer_pubkey>`");
					return (Some(response.to_string()), false);
				}
				let channel_id_vec = hex_utils::to_vec(channel_id_str.unwrap());
				if channel_id_vec.is_none() || channel_id_vec.as_ref().unwrap().len() != 32 {
					let response = ("ERROR: couldn't parse channel_id");
					return (Some(response.to_string()), false);
				}
				let mut channel_id = [0; 32];
				channel_id.copy_from_slice(&channel_id_vec.unwrap());

				let peer_pubkey_str = words.next();
				if peer_pubkey_str.is_none() {
					let response = ("ERROR: forceclosechannel requires a peer pubkey: `forceclosechannel <channel_id> <peer_pubkey>`");
					return (Some(response.to_string()), false);
				}
				let peer_pubkey_vec = match hex_utils::to_vec(peer_pubkey_str.unwrap()) {
					Some(peer_pubkey_vec) => peer_pubkey_vec,
					None => {
						let response = ("ERROR: couldn't parse peer_pubkey");
						return (Some(response.to_string()), false);
					}
				};
				let peer_pubkey = match PublicKey::from_slice(&peer_pubkey_vec) {
					Ok(peer_pubkey) => peer_pubkey,
					Err(_) => {
						let response = ("ERROR: couldn't parse peer_pubkey");
						return (Some(response.to_string()), false);
					}
				};

				let response = force_close_channel(channel_id, peer_pubkey, channel_manager.clone()).unwrap();
				return (Some(response.to_string()), false);
			}
			"nodeinfo" => {
				let response = node_info(&channel_manager, &peer_manager).unwrap();
				return (Some(response.to_string()), false);
			},
			"listpeers" => {
				let response = list_peers(peer_manager.clone()).unwrap();
				return (Some(response.to_string()), false);
			},
			"signmessage" => {
				const MSG_STARTPOS: usize = "signmessage".len() + 1;
				if line.as_bytes().len() <= MSG_STARTPOS {
					let response = ("ERROR: signmsg requires a message");
					return (Some(response.to_string()), false);
				}
				let response = format!(
					"{:?}",
					lightning::util::message_signing::sign(
						&line.as_bytes()[MSG_STARTPOS..],
						&keys_manager.get_node_secret_key()
					)
				);
				return (Some(response.to_string()), true);
			}
			"sendonionmessage" => {
				let path_pks_str = words.next();
				if path_pks_str.is_none() {
					let response = (
						"ERROR: sendonionmessage requires at least one node id for the path"
					);
					return (Some(response.to_string()), false);
				}
				let mut node_pks = Vec::new();
				let mut errored = false;
				let mut response = String::new();
				for pk_str in path_pks_str.unwrap().split(",") {
					let node_pubkey_vec = match hex_utils::to_vec(pk_str) {
						Some(peer_pubkey_vec) => peer_pubkey_vec,
						None => {
							response.push_str("ERROR: couldn't parse peer_pubkey");
							errored = true;
							break;
						}
					};
					let node_pubkey = match PublicKey::from_slice(&node_pubkey_vec) {
						Ok(peer_pubkey) => peer_pubkey,
						Err(_) => {
							response.push_str("ERROR: couldn't parse peer_pubkey");
							errored = true;
							break;
						}
					};
					node_pks.push(node_pubkey);
				}
				if errored {
					return (Some(response.to_string()), false);
				}
				let tlv_type = match words.next().map(|ty_str| ty_str.parse()) {
					Some(Ok(ty)) if ty >= 64 => ty,
					_ => {
						let response = ("Need an integral message type above 64");
						return (Some(response.to_string()), false);
					}
				};
				let data = match words.next().map(|s| hex_utils::to_vec(s)) {
					Some(Some(data)) => data,
					_ => {
						let response = ("Need a hex data string");
						return (Some(response.to_string()), false);
					}
				};
				let destination_pk = node_pks.pop().unwrap();
				match onion_messenger.send_onion_message(
					&node_pks,
					Destination::Node(destination_pk),
					OnionMessageContents::Custom(UserOnionMessageContents { tlv_type, data }),
					None,
				) {
					Ok(()) => {
						let response = ("SUCCESS: forwarded onion message to first hop");
						return (Some(response.to_string()), false);
					},
					Err(e) => {
						let response = format!("ERROR: failed to send onion message: {:?}", e);
						return (Some(response.to_string()), false);
					},
				}
			}
			"quit" | "exit" => {
				let response = ("Exiting node...");
				return (Some(response.to_string()), false)
			}
			_ => {
				let response = ("Unknown command. See `\"help\" for available commands.");
				return (Some(response.to_string()), false)
			},
		}
	}
	let response = "No command entered. See `\"help\" for available commands.";
	
	return (Some(response.to_string()), false)

}

fn help() {
	let package_version = env!("CARGO_PKG_VERSION");
	let package_name = env!("CARGO_PKG_NAME");
	println!("\nVERSION:");
	println!("  {} v{}", package_name, package_version);
	println!("\nUSAGE:");
	println!("  Command [arguments]");
	println!("\nCOMMANDS:");
	println!("  help\tShows a list of commands.");
	println!("  quit\tClose the application.");
	println!("\n  Channels:");
	println!("      openchannel pubkey@host:port <chan_amt_satoshis> <push_amt_msatoshis> <rgb_contract_id> <chan_amt_rgb> [--public]");
	println!("      closechannel <channel_id> <peer_pubkey>");
	println!("      forceclosechannel <channel_id> <peer_pubkey>");
	println!("      listchannels");
	println!("\n  Peers:");
	println!("      connectpeer pubkey@host:port");
	println!("      disconnectpeer <peer_pubkey>");
	println!("      listpeers");
	println!("\n  Payments:");
	println!("      sendpayment <invoice>");
	println!("      keysend <dest_pubkey> <amt_msats> <rgb_contract_id> <amt_rgb>");
	println!("      listpayments");
	println!("\n  Invoices:");
	println!("      getinvoice <amt_msats> <expiry_secs> <rgb_contract_id> <amt_rgb>");
	println!("      invoicestatus <invoice>");
	println!("\n  Onchain:");
	println!("      getaddress");
	println!("      listunspent");
	println!("\n  RGB:");
	println!("      createutxos");
	println!("      issueasset <supply> <ticker> <name> <precision>");
	println!("      assetbalance <contract_id>");
	println!("      sendasset <rgb_contract_id> <amt_rgb>");
	println!("      receiveasset");
	println!("      refresh");
	println!("\n  Other:");
	println!("      mine <num_blocks>");
	println!("      signmessage <message>");
	println!(
		"      sendonionmessage <node_id_1,node_id_2,..,destination_node_id> <type> <hex_bytes>"
	);
	println!("      nodeinfo");
}

fn node_info(channel_manager: &Arc<ChannelManager>, peer_manager: &Arc<PeerManager>) -> Option<String> {
	let mut response = String::new();
	response.push_str("\t{{\n");
	response.push_str(&format!("\t\t node_pubkey: {},\n", channel_manager.get_our_node_id()));
	let chans = channel_manager.list_channels();
	response.push_str(&format!("\t\t num_channels: {},\n", chans.len()));
	response.push_str(&format!("\t\t num_usable_channels: {},\n", chans.iter().filter(|c| c.is_usable).count()));
	let local_balance_msat = chans.iter().map(|c| c.balance_msat).sum::<u64>();
	response.push_str(&format!("\t\t local_balance_msat: {},\n", local_balance_msat));
	response.push_str(&format!("\t\t num_peers: {}\n", peer_manager.get_peer_node_ids().len()));
	response.push_str("\t}}\n");
	Some(response)
}

fn list_peers(peer_manager: Arc<PeerManager>) -> Option<String> {
	let mut response = String::new();
	response.push_str("[\n");
	for (pubkey, _) in peer_manager.get_peer_node_ids() {
		response.push_str("\n");
		response.push_str("\t{{\n");
		response.push_str(&format!("\t\t peer_pubkey: {pubkey}\n"));
		response.push_str("\t}},\n");
	}
	response.push_str("]\n");
	Some(response)
}

fn list_channels(
	channel_manager: &Arc<ChannelManager>, network_graph: &Arc<NetworkGraph>, ldk_data_dir: String,
) -> String {
	let mut response = String::new();
	response.push_str("[\n");
	for chan_info in channel_manager.list_channels() {
		response.push_str("\n");
		response.push_str("\t{{\n");
		response.push_str(&format!("\t\t channel_id: {},\n", hex_utils::hex_str(&chan_info.channel_id[..])));

		if let Some(funding_txo) = chan_info.funding_txo {
			response.push_str(&format!("\t\t funding_txid: {},\n", funding_txo.txid));
		}

		response.push_str(&format!("\t\tpeer_pubkey: {},\n", hex_utils::hex_str(&chan_info.counterparty.node_id.serialize())));

		if let Some(node_info) = network_graph
			.read_only()
			.nodes()
			.get(&NodeId::from_pubkey(&chan_info.counterparty.node_id))
		{
			if let Some(announcement) = &node_info.announcement_info {
				response.push_str(&format!("\t\tpeer_alias: {},\n", announcement.alias));
			}
		}

		if let Some(id) = chan_info.short_channel_id {
			response.push_str(&format!("\t\t short_channel_id: {},\n", id));
		}
		response.push_str(&format!("\t\tis_channel_ready: {},\n", chan_info.is_channel_ready));
		response.push_str(&format!("\t\tchannel_value_satoshis: {},\n", chan_info.channel_value_satoshis));
		response.push_str(&format!("\t\tlocal_balance_msat: {},\n", chan_info.balance_msat));
		if chan_info.is_usable {
			response.push_str(&format!("\t\tavailable_balance_for_send_msat: {},\n", chan_info.outbound_capacity_msat));
			response.push_str(&format!("\t\tavailable_balance_for_recv_msat: {},\n", chan_info.inbound_capacity_msat));
		}
		response.push_str(&format!("\t\tchannel_can_send_payments: {},\n", chan_info.is_usable));
		response.push_str(&format!("\t\tpublic: {},\n", chan_info.is_public));

		let ldk_data_dir_path = PathBuf::from(ldk_data_dir.clone());
		let info_file_path = ldk_data_dir_path.join(hex::encode(chan_info.channel_id));
		let (contract_id, local_rgb_amount, remote_rgb_amount) = if info_file_path.exists() {
			let (rgb_info, _) = get_rgb_channel_info(&chan_info.channel_id, &ldk_data_dir_path);
			(
				rgb_info.contract_id.to_string(),
				rgb_info.local_rgb_amount.to_string(),
				rgb_info.remote_rgb_amount.to_string(),
			)
		} else {
			let not_available = "N/A".to_string();
			(not_available.clone(), not_available.clone(), not_available)
		};
		response.push_str(&format!("\t\trgb_contract_id: {},\n", contract_id));
		response.push_str(&format!("\t\trgb_local_amount: {},\n", local_rgb_amount));
		response.push_str(&format!("\t\trgb_remote_amount: {}\n", remote_rgb_amount));
		response.push_str("\t}},\n");
	}
	response.push_str("]\n");
	response
}

fn invoice_status(inbound_payments: PaymentInfoStorage, invoice: Invoice) -> Option<String> {
	let inbound = inbound_payments.lock().unwrap();

	let payment_hash = PaymentHash(invoice.payment_hash().into_inner());
	match inbound.get(&payment_hash) {
		Some(v) => {
			let status_str = match v.status {
				HTLCStatus::Pending if invoice.is_expired() => "expired",
				HTLCStatus::Pending => "pending",
				HTLCStatus::Succeeded => "succeeded",
				HTLCStatus::Failed => "failed",
			};
			let response = format!("{}", status_str);
			return Some(response.to_string());
		}
		None => {
			let response = format!("ERROR: unknown invoice");
			return Some(response.to_string());
		}
	};
}

fn list_payments(
	inbound_payments: PaymentInfoStorage, outbound_payments: PaymentInfoStorage
) -> String {
	let inbound = inbound_payments.lock().unwrap();
	let outbound = outbound_payments.lock().unwrap();
	
	let mut response = String::new();
	response.push_str("[\n");

	for (payment_hash, payment_info) in inbound.deref() {
		response.push_str("\n");
		response.push_str("\t{{\n");
		response.push_str(&format!("\t\t amount_millisatoshis: {},\n", payment_info.amt_msat));
		response.push_str(&format!("\t\t payment_hash: {},\n", hex_utils::hex_str(&payment_hash.0)));
		response.push_str("\t\t htlc_direction: inbound,\n");
		response.push_str(&format!("\t\t htlc_status: {}\n", match payment_info.status {
			HTLCStatus::Pending => "pending",
			HTLCStatus::Succeeded => "succeeded",
			HTLCStatus::Failed => "failed",
		}));
		response.push_str("\t}},\n");
	}

	for (payment_hash, payment_info) in outbound.deref() {
		response.push_str("\n");
		response.push_str("\t{{\n");
		response.push_str(&format!("\t\t amount_millisatoshis: {},\n", payment_info.amt_msat));
		response.push_str(&format!("\t\t payment_hash: {},\n", hex_utils::hex_str(&payment_hash.0)));
		response.push_str("\t\t htlc_direction: outbound,\n");
		response.push_str(&format!("\t\t htlc_status: {}\n", match payment_info.status {
			HTLCStatus::Pending => "pending",
			HTLCStatus::Succeeded => "succeeded",
			HTLCStatus::Failed => "failed",
		}));
		response.push_str("\t}},\n");
	}
	response.push_str("]\n");
	response
}

pub(crate) async fn connect_peer_if_necessary(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
) -> Result<(), ()> {
	for (node_pubkey, _) in peer_manager.get_peer_node_ids() {
		if node_pubkey == pubkey {
			return Ok(());
		}
	}
	let res = do_connect_peer(pubkey, peer_addr, peer_manager).await;
	if res.is_err() {
		println!("ERROR: failed to connect to peer");
	}
	res
}

pub(crate) async fn do_connect_peer(
	pubkey: PublicKey, peer_addr: SocketAddr, peer_manager: Arc<PeerManager>,
) -> Result<(), ()> {
	match lightning_net_tokio::connect_outbound(Arc::clone(&peer_manager), pubkey, peer_addr).await
	{
		Some(connection_closed_future) => {
			let mut connection_closed_future = Box::pin(connection_closed_future);
			loop {
				tokio::select! {
					_ = &mut connection_closed_future => return Err(()),
					_ = tokio::time::sleep(Duration::from_millis(10)) => {},
				};
				if peer_manager.get_peer_node_ids().iter().find(|(id, _)| *id == pubkey).is_some() {
					return Ok(());
				}
			}
		}
		None => Err(()),
	}
}

fn do_disconnect_peer(
	pubkey: bitcoin::secp256k1::PublicKey, peer_manager: Arc<PeerManager>,
	channel_manager: Arc<ChannelManager>,
) -> Result<(), ()> {
	//check for open channels with peer
	for channel in channel_manager.list_channels() {
		if channel.counterparty.node_id == pubkey {
			println!("Error: Node has an active channel with this peer, close any channels first");
			return Err(());
		}
	}

	//check the pubkey matches a valid connected peer
	let peers = peer_manager.get_peer_node_ids();
	if !peers.iter().any(|(pk, _)| &pubkey == pk) {
		println!("Error: Could not find peer {}", pubkey);
		return Err(());
	}

	peer_manager.disconnect_by_node_id(pubkey);
	Ok(())
}

fn open_channel(
	peer_pubkey: PublicKey, channel_amt_sat: u64, push_amt_msat: u64, announced_channel: bool,
	channel_manager: Arc<ChannelManager>, proxy_endpoint: &str,
) -> Result<[u8; 32], ()> {
	let config = UserConfig {
		channel_handshake_limits: ChannelHandshakeLimits {
			// lnd's max to_self_delay is 2016, so we want to be compatible.
			their_to_self_delay: 2016,
			..Default::default()
		},
		channel_handshake_config: ChannelHandshakeConfig {
			announced_channel,
			our_htlc_minimum_msat: HTLC_MIN_MSAT,
			..Default::default()
		},
		..Default::default()
	};

	let consignment_endpoint = RgbTransport::from_str(proxy_endpoint).unwrap();
	match channel_manager.create_channel(
		peer_pubkey,
		channel_amt_sat,
		push_amt_msat,
		0,
		Some(config),
		consignment_endpoint,
	) {
		Ok(temporary_channel_id) => {
			println!("EVENT: initiated channel with peer {}. ", peer_pubkey);
			return Ok(temporary_channel_id);
		}
		Err(e) => {
			println!("ERROR: failed to open channel: {:?}", e);
			return Err(());
		}
	}
}

fn send_payment(
	channel_manager: &ChannelManager, invoice: &Invoice, payment_storage: PaymentInfoStorage,
	ldk_data_dir: PathBuf,
) {
	let payment_hash = PaymentHash(invoice.payment_hash().clone().into_inner());
	write_rgb_payment_info_file(
		&ldk_data_dir,
		&payment_hash,
		invoice.rgb_contract_id().unwrap(),
		invoice.rgb_amount().unwrap(),
	);
	let status =
		match pay_invoice(invoice, Retry::Timeout(Duration::from_secs(10)), channel_manager) {
			Ok(_payment_id) => {
				let payee_pubkey = invoice.recover_payee_pub_key();
				let amt_msat = invoice.amount_milli_satoshis().unwrap();
				println!("EVENT: initiated sending {} msats to {}", amt_msat, payee_pubkey);
				print!("> ");
				HTLCStatus::Pending
			}
			Err(e) => {
				println!("ERROR: failed to send payment: {:?}", e);
				print!("> ");
				HTLCStatus::Failed
			}
		};
	let payment_secret = Some(invoice.payment_secret().clone());

	let mut payments = payment_storage.lock().unwrap();
	payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: payment_secret,
			status,
			amt_msat: MillisatAmount(invoice.amount_milli_satoshis()),
		},
	);
}

fn keysend<E: EntropySource>(
	channel_manager: &ChannelManager, payee_pubkey: PublicKey, amt_msat: u64, entropy_source: &E,
	payment_storage: PaymentInfoStorage, contract_id: ContractId, amt_rgb: u64,
	ldk_data_dir: PathBuf,
) {
	let payment_preimage = PaymentPreimage(entropy_source.get_secure_random_bytes());
	let payment_hash = PaymentHash(Sha256::hash(&payment_preimage.0[..]).into_inner());
	write_rgb_payment_info_file(&ldk_data_dir, &payment_hash, contract_id, amt_rgb);

	let route_params = RouteParameters {
		payment_params: PaymentParameters::for_keysend(payee_pubkey, 40),
		final_value_msat: amt_msat,
	};
	let status = match channel_manager.send_spontaneous_payment_with_retry(
		Some(payment_preimage),
		RecipientOnionFields::spontaneous_empty(),
		PaymentId(payment_hash.0),
		route_params,
		Retry::Timeout(Duration::from_secs(10)),
	) {
		Ok(_payment_hash) => {
			println!("EVENT: initiated sending {} msats to {}", amt_msat, payee_pubkey);
			print!("> ");
			HTLCStatus::Pending
		}
		Err(e) => {
			println!("ERROR: failed to send payment: {:?}", e);
			print!("> ");
			HTLCStatus::Failed
		}
	};

	let mut payments = payment_storage.lock().unwrap();
	payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: None,
			status,
			amt_msat: MillisatAmount(Some(amt_msat)),
		},
	);
}


#[derive(Serialize, Deserialize, Debug, Clone)]
struct InvoiceString {
	invoice_string: String,
}

fn get_invoice(
	amt_msat: u64, payment_storage: PaymentInfoStorage, channel_manager: &ChannelManager,
	keys_manager: Arc<KeysManager>, network: Network, expiry_secs: u32,
	logger: Arc<disk::FilesystemLogger>, contract_id: ContractId, amt_rgb: u64,
) -> Option<String> {
	let response: String;
	let mut payments = payment_storage.lock().unwrap();
	let currency = match network {
		Network::Bitcoin => Currency::Bitcoin,
		Network::Testnet => Currency::BitcoinTestnet,
		Network::Regtest => Currency::Regtest,
		Network::Signet => Currency::Signet,
	};
	let invoice = match utils::create_invoice_from_channelmanager(
		channel_manager,
		keys_manager,
		logger,
		currency,
		Some(amt_msat),
		"ldk-tutorial-node".to_string(),
		expiry_secs,
		None,
		Some(contract_id),
		Some(amt_rgb),
	) {
		Ok(inv) => {
			response = serde_json::to_string(&InvoiceString {invoice_string: inv.to_string()}).unwrap();
			println!("SUCCESS: generated invoice: {}", inv);
			inv
		}
		Err(e) => {
			response = format!("ERROR: failed to create invoice: {:?}", e);
			println!("{}", response);
			return Some(response);
		}
	};

	let payment_hash = PaymentHash(invoice.payment_hash().clone().into_inner());
	payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: Some(invoice.payment_secret().clone()),
			status: HTLCStatus::Pending,
			amt_msat: MillisatAmount(Some(amt_msat)),
		},
	);

	return Some(response);
}

fn close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) -> Option<String> {
	let mut response = String::new();
	match channel_manager.close_channel(&channel_id, &counterparty_node_id) {
		Ok(()) => {
			response.push_str(&format!("EVENT: initiating channel close"));
		},
		Err(e) => {
			response.push_str(&format!("ERROR: failed to close channel: {:?}", e));
		},
	}
	return Some(response.to_string());
}

fn force_close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) -> Option<String> {
	let mut response = String::new();
	match channel_manager.force_close_broadcasting_latest_txn(&channel_id, &counterparty_node_id) {
		Ok(()) => {
			response.push_str(&format!("EVENT: initiating channel force-close"));
		},
		Err(e) => {
			response.push_str(&format!("ERROR: failed to force-close channel: {:?}", e));
		},
	}
	return Some(response.to_string());
}

pub(crate) fn parse_peer_info(
	peer_pubkey_and_ip_addr: String,
) -> Result<(PublicKey, SocketAddr), std::io::Error> {
	let mut pubkey_and_addr = peer_pubkey_and_ip_addr.split("@");
	let pubkey = pubkey_and_addr.next();
	let peer_addr_str = pubkey_and_addr.next();
	if peer_addr_str.is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: incorrectly formatted peer info. Should be formatted as: `pubkey@host:port`",
		));
	}

	let peer_addr = peer_addr_str.unwrap().to_socket_addrs().map(|mut r| r.next());
	if peer_addr.is_err() || peer_addr.as_ref().unwrap().is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: couldn't parse pubkey@host:port into a socket address",
		));
	}

	let pubkey = hex_utils::to_compressed_pubkey(pubkey.unwrap());
	if pubkey.is_none() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::Other,
			"ERROR: unable to parse given pubkey for node",
		));
	}

	Ok((pubkey.unwrap(), peer_addr.unwrap().unwrap()))
}

pub(crate) async fn check_uncolored_utxos(ldk_data_dir: &str) -> Result<(), Error> {
	let rgb_utxos_path = format!("{}/rgb_utxos", ldk_data_dir);
	let serialized_utxos = fs::read_to_string(rgb_utxos_path).expect("able to read rgb utxos file");
	let rgb_utxos: RgbUtxos = serde_json::from_str(&serialized_utxos).expect("valid rgb utxos");
	let utxo = rgb_utxos.utxos.iter().find(|u| !u.colored);
	match utxo {
		Some(_) => Ok(()),
		None => Err(Error::NoAvailableUtxos),
	}
}

pub(crate) async fn get_utxo(ldk_data_dir: &str) -> RgbUtxo {
	let rgb_utxos_path = format!("{}/rgb_utxos", ldk_data_dir);
	let serialized_utxos = fs::read_to_string(rgb_utxos_path).expect("able to read rgb utxos file");
	let rgb_utxos: RgbUtxos = serde_json::from_str(&serialized_utxos).expect("valid rgb utxos");
	rgb_utxos.utxos.into_iter().find(|u| !u.colored).expect("at least one unlocked UTXO")
}

pub(crate) async fn mine(bitcoind_client: &BitcoindClient, num_blocks: u16) {
	let address = bitcoind_client.get_new_address().await.to_string();
	bitcoind_client.generate_to_adress(num_blocks, address).await;
}


// hodl invoice

#[derive(Clone, Debug)]
struct TestPreimageData {
	payment_preimage: PaymentPreimage,
	payment_hash: PaymentHash,
}

fn get_hodl_invoice(
	amt_msat: u64, payment_storage: PaymentInfoStorage, channel_manager: &ChannelManager,
	keys_manager: Arc<KeysManager>, network: Network, expiry_secs: u32,
	logger: Arc<disk::FilesystemLogger>, contract_id: ContractId, amt_rgb: u64, payment_hash: String,
) -> String {
	let mut payments = payment_storage.lock().unwrap();
	let currency = match network {
		Network::Bitcoin => Currency::Bitcoin,
		Network::Testnet => Currency::BitcoinTestnet,
		Network::Regtest => Currency::Regtest,
		Network::Signet => Currency::Signet,
	};
	let response: String;
	let payment_hash = PaymentHash(hex::decode(payment_hash).unwrap().try_into().expect("payment hash string slice has incorrect length"));
	use std::time::SystemTime;
	let invoice = match utils::create_invoice_from_channelmanager_and_duration_since_epoch_with_payment_hash(
		channel_manager,
		keys_manager,
		logger,
		currency,
		Some(amt_msat),
		"ldk-tutorial-node".to_string(),
		Duration::new(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(), 0),
		expiry_secs,
		payment_hash,
		None,
		Some(contract_id),
		Some(amt_rgb),
	)
	{
		Ok(inv) => {
			response = serde_json::to_string(&InvoiceString {invoice_string: inv.to_string()}).unwrap();
			println!("{}", response);
			inv
		}
		Err(e) => {
			response = format!("ERROR: failed to create invoice: {:?}", e);
			println!("{}", response);
			return response;
		}
	};

	let payment_hash = PaymentHash(invoice.payment_hash().clone().into_inner());
	payments.insert(
		payment_hash,
		PaymentInfo {
			preimage: None,
			secret: Some(invoice.payment_secret().clone()),
			status: HTLCStatus::Pending,
			amt_msat: MillisatAmount(Some(amt_msat)),
		},
	);
	
	return response;
}

fn settle_hodl_invoice(
	preimage: String, 
	channel_manager: &ChannelManager,
) -> String {

	channel_manager.claim_funds(PaymentPreimage(hex::decode(preimage).unwrap().try_into().expect("preimage string slice has incorrect length")));

	return "Payment claim seems to have succeeded! The invoice should be settled".to_string()
} 