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

const MIN_CREATE_UTXOS_SATS: u64 = 10000;
const UTXO_NUM: u8 = 10;

const OPENCHANNEL_MIN_SAT: u64 = 5000;
const OPENCHANNEL_MAX_SAT: u64 = 16777215;

const DUST_LIMIT_MSAT: u64 = 546000;

const HTLC_MIN_MSAT: u64 = 3000000;

const INVOICE_MIN_MSAT: u64 = HTLC_MIN_MSAT;

use actix_web::{ self, web::{self, Data}, App, HttpResponse, HttpServer, Responder };
pub use serde_json::{ self, Map, Value };

use crate::request::{ self, AppDataStruct };

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


pub(crate) async fn poll_for_user_input(
	shared_data: AppDataStruct,
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

		let response = request::handle_request(line, shared_data.clone()).await;

		if response == "quitting" {
			break
		}

	}
}

