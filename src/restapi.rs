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
use crate::seal::Revealed;
use crate::{
	ChannelManager, HTLCStatus, MillisatAmount, NetworkGraph, OnionMessenger, PaymentInfo,
	PaymentInfoStorage, PeerManager,
};
use crate::{FEE_RATE, UTXO_SIZE_SAT};
use amplify::bmap;
use bdk::bitcoin::hashes::Hash;
use bdk::bitcoin::OutPoint;
use bdk::database::SqliteDatabase;
use bdk::{FeeRate, SignOptions, Wallet};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::PublicKey;
use bp::seals::txout::ExplicitSeal;
use bp::seals::txout::{blind::ConcealedSeal, CloseMethod};
use invoice::ConsignmentEndpoint;
use lightning::chain::keysinterface::{EntropySource, KeysManager};
use lightning::ln::channelmanager::{PaymentId, RecipientOnionFields, Retry};
use lightning::ln::msgs::NetAddress;
use lightning::ln::{PaymentHash, PaymentPreimage};
use lightning::onion_message::{CustomOnionMessageContents, Destination, OnionMessageContents};
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
use rgb::fungible::allocation::AllocatedValue;
use rgb::Contract;
use rgb::ContractId;
use rgb::EndpointValueMap;
use rgb::SealEndpoint;
use rgb::{seal, StateTransfer};
use rgb_rpc::Client;
use rgb_rpc::ContractValidity;
use rgb_rpc::Reveal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io;
use std::io::Write;
use std::iter::FromIterator;
use std::net::{SocketAddr, ToSocketAddrs};
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stens::AsciiString;
use strict_encoding::strict_deserialize;
use strict_encoding::strict_serialize;
use strict_encoding::StrictEncode;

use actix_web::{ self, get, post, delete, rt, web::{self, Data}, App, HttpResponse, HttpServer, Responder };
pub use serde_json::{ self, Map, Value };

const MIN_CREATE_UTXOS_SATS: u64 = 10000;
const UTXO_NUM: u8 = 10;

const OPENCHANNEL_MIN_SAT: u64 = 5000;
const OPENCHANNEL_MAX_SAT: u64 = 16777215;

const DUST_LIMIT_MSAT: u64 = 546000;

const HTLC_MIN_MSAT: u64 = 3000000;

const INVOICE_MIN_MSAT: u64 = HTLC_MIN_MSAT;

use crate::request::{self, AppDataStruct};

#[derive(Serialize, Deserialize)]
struct BlindedInfo {
	contract_id: Option<ContractId>,
	seal: seal::Revealed,
	consumed: bool,
}

pub(crate) struct LdkUserInfo {
	pub(crate) bitcoind_rpc_username: String,
	pub(crate) bitcoind_rpc_password: String,
	pub(crate) bitcoind_rpc_port: u16,
	pub(crate) bitcoind_rpc_host: String,
	pub(crate) ldk_storage_dir_path: String,
	pub(crate) rgb_node_port: u16,
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



pub(crate) fn start_api(
	shared_data: AppDataStruct,
	rest_api_port: u16,
) -> std::io::Result<()> 
{
	let shared_data: Data<Mutex<AppDataStruct>> = Data::new(Mutex::new(shared_data));
	
	println!("Starting REST API on port {}", rest_api_port);

	rt::System::new().block_on(
		HttpServer::new(move || {
			App::new()
				.app_data(shared_data.clone())
				.service(cliwrapper)
		})
		.bind(("0.0.0.0", rest_api_port))?
		.run()
	)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CliCommands {
	pub commands: String,
}

// generalized API wrapper around the cli functions 
#[post("/cliwrapper")]
async fn cliwrapper(
    request: String, 
	shared_data: Data<Mutex<AppDataStruct>>,
) -> impl Responder 
{
	let shared_data: &mut AppDataStruct = &mut *shared_data.lock().unwrap();
	

	// Parse request
	println!("Raw request: {:?}", request);
	let parsed_request: CliCommands = serde_json::from_str(&request).unwrap(); 
	println!("Parsed request: {:?}", parsed_request);
	let line: String = parsed_request.commands;
	
	let response = request::handle_request(line, shared_data.clone()).await;

	HttpResponse::Ok().body(response)
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

fn node_info(channel_manager: &Arc<ChannelManager>, peer_manager: &Arc<PeerManager>) {
	println!("\t{{");
	println!("\t\t node_pubkey: {}", channel_manager.get_our_node_id());
	let chans = channel_manager.list_channels();
	println!("\t\t num_channels: {}", chans.len());
	println!("\t\t num_usable_channels: {}", chans.iter().filter(|c| c.is_usable).count());
	let local_balance_msat = chans.iter().map(|c| c.balance_msat).sum::<u64>();
	println!("\t\t local_balance_msat: {}", local_balance_msat);
	println!("\t\t num_peers: {}", peer_manager.get_peer_node_ids().len());
	println!("\t}},");
}

fn list_peers(peer_manager: Arc<PeerManager>) {
	println!("\t{{");
	for (pubkey, _) in peer_manager.get_peer_node_ids() {
		println!("\t\t pubkey: {}", pubkey);
	}
	println!("\t}},");
}

fn list_channels(
	channel_manager: &Arc<ChannelManager>, network_graph: &Arc<NetworkGraph>, ldk_data_dir: String,
) {
	print!("[");
	for chan_info in channel_manager.list_channels() {
		println!("");
		println!("\t{{");
		println!("\t\tchannel_id: {},", hex_utils::hex_str(&chan_info.channel_id[..]));
		if let Some(funding_txo) = chan_info.funding_txo {
			println!("\t\tfunding_txid: {},", funding_txo.txid);
		}

		println!(
			"\t\tpeer_pubkey: {},",
			hex_utils::hex_str(&chan_info.counterparty.node_id.serialize())
		);
		if let Some(node_info) = network_graph
			.read_only()
			.nodes()
			.get(&NodeId::from_pubkey(&chan_info.counterparty.node_id))
		{
			if let Some(announcement) = &node_info.announcement_info {
				println!("\t\tpeer_alias: {}", announcement.alias);
			}
		}

		if let Some(id) = chan_info.short_channel_id {
			println!("\t\tshort_channel_id: {},", id);
		}
		println!("\t\tis_channel_ready: {},", chan_info.is_channel_ready);
		println!("\t\tchannel_value_satoshis: {},", chan_info.channel_value_satoshis);
		println!("\t\tlocal_balance_msat: {},", chan_info.balance_msat);
		if chan_info.is_usable {
			println!("\t\tavailable_balance_for_send_msat: {},", chan_info.outbound_capacity_msat);
			println!("\t\tavailable_balance_for_recv_msat: {},", chan_info.inbound_capacity_msat);
		}
		println!("\t\tchannel_can_send_payments: {},", chan_info.is_usable);
		println!("\t\tpublic: {},", chan_info.is_public);

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
		println!("\t\trgb_contract_id: {contract_id},");
		println!("\t\trgb_local_amount: {local_rgb_amount},");
		println!("\t\trgb_remote_amount: {remote_rgb_amount},");
		println!("\t}},");
	}
	println!("]");
}

fn invoice_status(inbound_payments: PaymentInfoStorage, invoice: Invoice) {
	let inbound = inbound_payments.lock().unwrap();

	let payment_hash = PaymentHash(invoice.payment_hash().clone().into_inner());
	match inbound.get(&payment_hash) {
		Some(v) => {
			let status_str = match v.status {
				HTLCStatus::Pending if invoice.is_expired() => "expired",
				HTLCStatus::Pending => "pending",
				HTLCStatus::Succeeded => "succeeded",
				HTLCStatus::Failed => "failed",
			};
			println!("{}", status_str);
		}
		None => {
			println!("ERROR: unknown invoice");
		}
	};
}

fn list_payments(inbound_payments: PaymentInfoStorage, outbound_payments: PaymentInfoStorage) {
	let inbound = inbound_payments.lock().unwrap();
	let outbound = outbound_payments.lock().unwrap();
	print!("[");
	for (payment_hash, payment_info) in inbound.deref() {
		println!("");
		println!("\t{{");
		println!("\t\tamount_millisatoshis: {},", payment_info.amt_msat);
		println!("\t\tpayment_hash: {},", hex_utils::hex_str(&payment_hash.0));
		println!("\t\thtlc_direction: inbound,");
		println!(
			"\t\thtlc_status: {},",
			match payment_info.status {
				HTLCStatus::Pending => "pending",
				HTLCStatus::Succeeded => "succeeded",
				HTLCStatus::Failed => "failed",
			}
		);

		println!("\t}},");
	}

	for (payment_hash, payment_info) in outbound.deref() {
		println!("");
		println!("\t{{");
		println!("\t\tamount_millisatoshis: {},", payment_info.amt_msat);
		println!("\t\tpayment_hash: {},", hex_utils::hex_str(&payment_hash.0));
		println!("\t\thtlc_direction: outbound,");
		println!(
			"\t\thtlc_status: {},",
			match payment_info.status {
				HTLCStatus::Pending => "pending",
				HTLCStatus::Succeeded => "succeeded",
				HTLCStatus::Failed => "failed",
			}
		);

		println!("\t}},");
	}
	println!("]");
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
	channel_manager: Arc<ChannelManager>, proxy_url: &str,
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

	let consignment_endpoint =
		ConsignmentEndpoint::from_str(&format!("rgbhttpjsonrpc:{}", proxy_url)).unwrap();
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

fn get_invoice(
	amt_msat: u64, payment_storage: PaymentInfoStorage, channel_manager: &ChannelManager,
	keys_manager: Arc<KeysManager>, network: Network, expiry_secs: u32,
	logger: Arc<disk::FilesystemLogger>, contract_id: ContractId, amt_rgb: u64,
) {
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
			println!("SUCCESS: generated invoice: {}", inv);
			inv
		}
		Err(e) => {
			println!("ERROR: failed to create invoice: {:?}", e);
			return;
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
}

fn close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) {
	match channel_manager.close_channel(&channel_id, &counterparty_node_id) {
		Ok(()) => println!("EVENT: initiating channel close"),
		Err(e) => println!("ERROR: failed to close channel: {:?}", e),
	}
}

fn force_close_channel(
	channel_id: [u8; 32], counterparty_node_id: PublicKey, channel_manager: Arc<ChannelManager>,
) {
	match channel_manager.force_close_broadcasting_latest_txn(&channel_id, &counterparty_node_id) {
		Ok(()) => println!("EVENT: initiating channel force-close"),
		Err(e) => println!("ERROR: failed to force-close channel: {:?}", e),
	}
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