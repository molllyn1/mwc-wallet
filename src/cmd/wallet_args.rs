// Copyright 2019 The Grin Developers
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

use crate::api::TLSConfig;
use crate::cli::command_loop;
use crate::cmd::wallet_args::ParseError::ArgumentError;
use crate::config::GRIN_WALLET_DIR;
use crate::util::file::get_first_line;
use crate::util::secp::key::SecretKey;
use crate::util::{Mutex, ZeroingString};

/// Argument parsing and error handling for wallet commands
use clap::ArgMatches;
use ed25519_dalek::SecretKey as DalekSecretKey;
use failure::Fail;
use grin_wallet_api::Owner;
use grin_wallet_config::parse_node_address_string;
use grin_wallet_config::{MQSConfig, TorConfig, WalletConfig};
use grin_wallet_controller::command;
use grin_wallet_controller::{Error, ErrorKind};
use grin_wallet_impls::tor::config::is_tor_address;
use grin_wallet_impls::{DefaultLCProvider, DefaultWalletImpl};
use grin_wallet_impls::{PathToSlateGetter, SlateGetter};
use grin_wallet_libwallet::proof::proofaddress;
use grin_wallet_libwallet::proof::proofaddress::ProvableAddress;
use grin_wallet_libwallet::Slate;
use grin_wallet_libwallet::{
	IssueInvoiceTxArgs, NodeClient, SwapStartArgs, WalletInst, WalletLCProvider,
};
use grin_wallet_util::grin_core as core;
use grin_wallet_util::grin_core::core::amount_to_hr_string;
use grin_wallet_util::grin_core::global;
use grin_wallet_util::grin_keychain as keychain;
use linefeed::terminal::Signal;
use linefeed::{Interface, ReadResult};
use rpassword;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

// define what to do on argument error
macro_rules! arg_parse {
	( $r:expr ) => {
		match $r {
			Ok(res) => res,
			Err(e) => {
				return Err(ErrorKind::ArgumentError(format!("{}", e)).into());
			}
		}
	};
}
/// Simple error definition, just so we can return errors from all commands
/// and let the caller figure out what to do
#[derive(Clone, Eq, PartialEq, Debug, Fail)]

pub enum ParseError {
	#[fail(display = "Invalid Arguments: {}", _0)]
	ArgumentError(String),
	#[fail(display = "Parsing IO error: {}", _0)]
	IOError(String),
	#[fail(display = "Wallet configuration already exists: {}", _0)]
	WalletExists(String),
	#[fail(display = "User Cancelled")]
	CancelledError,
}

impl From<std::io::Error> for ParseError {
	fn from(e: std::io::Error) -> ParseError {
		ParseError::IOError(format!("{}", e))
	}
}

fn prompt_password_stdout(prompt: &str) -> ZeroingString {
	ZeroingString::from(rpassword::prompt_password_stdout(prompt).unwrap())
}

pub fn prompt_password(password: &Option<ZeroingString>) -> ZeroingString {
	match password {
		None => prompt_password_stdout("Password: "),
		Some(p) => p.clone(),
	}
}

fn prompt_password_confirm() -> ZeroingString {
	let mut first = ZeroingString::from("first");
	let mut second = ZeroingString::from("second");
	while first != second {
		first = prompt_password_stdout("Password: ");
		second = prompt_password_stdout("Confirm Password: ");
	}
	first
}

fn prompt_recovery_phrase<L, C, K>(
	wallet: Arc<Mutex<Box<dyn WalletInst<'static, L, C, K>>>>,
) -> Result<ZeroingString, ParseError>
where
	DefaultWalletImpl<'static, C>: WalletInst<'static, L, C, K>,
	L: WalletLCProvider<'static, C, K>,
	C: NodeClient + 'static,
	K: keychain::Keychain + 'static,
{
	let interface = Arc::new(Interface::new("recover")?);
	let mut phrase = ZeroingString::from("");
	interface.set_report_signal(Signal::Interrupt, true);
	interface.set_prompt("phrase> ")?;
	loop {
		println!("Please enter your recovery phrase:");
		let res = interface.read_line()?;
		match res {
			ReadResult::Eof => break,
			ReadResult::Signal(sig) => {
				if sig == Signal::Interrupt {
					interface.cancel_read_line()?;
					return Err(ParseError::CancelledError);
				}
			}
			ReadResult::Input(line) => {
				let mut w_lock = wallet.lock();
				let p = w_lock.lc_provider().unwrap();
				if p.validate_mnemonic(ZeroingString::from(line.clone()))
					.is_ok()
				{
					phrase = ZeroingString::from(line);
					break;
				} else {
					println!();
					println!("Recovery word phrase is invalid.");
					println!();
					interface.set_buffer(&line)?;
				}
			}
		}
	}
	Ok(phrase)
}

fn prompt_pay_invoice(slate: &Slate, method: &str, dest: &str) -> Result<bool, ParseError> {
	let interface = Arc::new(Interface::new("pay")?);
	let amount = amount_to_hr_string(slate.amount, false);
	interface.set_report_signal(Signal::Interrupt, true);
	interface.set_prompt(
		"To proceed, type the exact amount of the invoice as displayed above (or Q/q to quit) > ",
	)?;
	println!();
	println!(
		"This command will pay the amount specified in the invoice using your wallet's funds."
	);
	println!("After you confirm, the following will occur: ");
	println!();
	println!(
		"* {} of your wallet funds will be added to the transaction to pay this invoice.",
		amount
	);
	if method == "http" {
		println!("* The resulting transaction will IMMEDIATELY be sent to the wallet listening at: '{}'.", dest);
	} else {
		println!("* The resulting transaction will be saved to the file '{}', which you can manually send back to the invoice creator.", dest);
	}
	println!();
	println!("The invoice slate's participant info is:");
	for m in slate.participant_messages().messages {
		println!("{}", m);
	}
	println!("Please review the above information carefully before proceeding");
	println!();
	loop {
		let res = interface.read_line()?;
		match res {
			ReadResult::Eof => return Ok(false),
			ReadResult::Signal(sig) => {
				if sig == Signal::Interrupt {
					interface.cancel_read_line()?;
					return Err(ParseError::CancelledError);
				}
			}
			ReadResult::Input(line) => {
				match line.trim() {
					"Q" | "q" => return Err(ParseError::CancelledError),
					result => {
						if result == amount {
							return Ok(true);
						} else {
							println!("Please enter exact amount of the invoice as shown above or Q to quit");
							println!();
						}
					}
				}
			}
		}
	}
}

// instantiate wallet (needed by most functions)

pub fn inst_wallet<L, C, K>(
	config: WalletConfig,
	node_client: C,
) -> Result<Arc<Mutex<Box<dyn WalletInst<'static, L, C, K>>>>, ParseError>
where
	DefaultWalletImpl<'static, C>: WalletInst<'static, L, C, K>,
	L: WalletLCProvider<'static, C, K>,
	C: NodeClient + 'static,
	K: keychain::Keychain + 'static,
{
	let mut wallet = Box::new(DefaultWalletImpl::<'static, C>::new(node_client.clone()).unwrap())
		as Box<dyn WalletInst<'static, L, C, K>>;
	let lc = wallet.lc_provider().unwrap();
	let _ = lc.set_top_level_directory(&config.data_file_dir);
	Ok(Arc::new(Mutex::new(wallet)))
}

// parses a required value, or throws error with message otherwise
fn parse_required<'a>(args: &'a ArgMatches, name: &str) -> Result<&'a str, ParseError> {
	let arg = args.value_of(name);
	match arg {
		Some(ar) => Ok(ar),
		None => {
			let msg = format!("Value for argument '{}' is required in this context", name,);
			Err(ParseError::ArgumentError(msg))
		}
	}
}

// parses a number, or throws error with message otherwise
fn parse_u64(arg: &str, name: &str) -> Result<u64, ParseError> {
	let val = arg.parse::<u64>();
	match val {
		Ok(v) => Ok(v),
		Err(e) => {
			let msg = format!("Could not parse {} as a whole number. e={}", name, e);
			Err(ParseError::ArgumentError(msg))
		}
	}
}

// parses a number, or throws error with message otherwise
fn parse_f32(arg: &str, name: &str) -> Result<f32, ParseError> {
	let val = arg.parse::<f32>();
	match val {
		Ok(v) => Ok(v),
		Err(e) => {
			let msg = format!("Could not parse {} as a decimal number. e={}", name, e);
			Err(ParseError::ArgumentError(msg))
		}
	}
}

// As above, but optional
fn parse_u64_or_none(arg: Option<&str>) -> Option<u64> {
	let val = match arg {
		Some(a) => a.parse::<u64>(),
		None => return None,
	};
	match val {
		Ok(v) => Some(v),
		Err(_) => None,
	}
}

pub fn parse_global_args(
	config: &WalletConfig,
	args: &ArgMatches,
) -> Result<command::GlobalArgs, ParseError> {
	let account = parse_required(args, "account")?;
	let mut show_spent = false;
	if args.is_present("show_spent") {
		show_spent = true;
	}
	let api_secret = get_first_line(config.api_secret_path.clone());
	let node_api_secret = get_first_line(config.node_api_secret_path.clone());
	let password = match args.value_of("pass") {
		None => None,
		Some(p) => Some(ZeroingString::from(p)),
	};

	let tls_conf = match config.tls_certificate_file.clone() {
		None => None,
		Some(file) => {
			let key = match config.tls_certificate_key.clone() {
				Some(k) => k,
				None => {
					let msg = format!("Private key for certificate is not set");
					return Err(ParseError::ArgumentError(msg));
				}
			};
			Some(TLSConfig::new(file, key))
		}
	};

	let chain_type = match config.chain_type.clone() {
		None => global::get_chain_type(),
		Some(c) => c,
	};

	Ok(command::GlobalArgs {
		account: account.to_owned(),
		show_spent: show_spent,
		chain_type: chain_type,
		api_secret: api_secret,
		node_api_secret: node_api_secret,
		password: password,
		tls_conf: tls_conf,
	})
}

pub fn parse_init_args<L, C, K>(
	wallet: Arc<Mutex<Box<dyn WalletInst<'static, L, C, K>>>>,
	config: &WalletConfig,
	g_args: &command::GlobalArgs,
	args: &ArgMatches,
	test_mode: bool,
) -> Result<command::InitArgs, ParseError>
where
	DefaultWalletImpl<'static, C>: WalletInst<'static, L, C, K>,
	L: WalletLCProvider<'static, C, K>,
	C: NodeClient + 'static,
	K: keychain::Keychain + 'static,
{
	// Checking is wallet data
	let mut wallet_data_path = PathBuf::from(&config.data_file_dir);
	wallet_data_path.push(
		config
			.wallet_data_dir
			.clone()
			.unwrap_or(GRIN_WALLET_DIR.to_string()),
	);
	if wallet_data_path.exists() && !test_mode {
		return Err(ParseError::WalletExists(
			wallet_data_path.to_str().unwrap_or("unknown").to_string(),
		));
	}

	let list_length = match args.is_present("short_wordlist") {
		false => 32,
		true => 16,
	};
	let recovery_phrase = match args.is_present("recover") {
		true => Some(prompt_recovery_phrase(wallet)?),
		false => None,
	};

	if recovery_phrase.is_some() {
		println!("Please provide a new password for the recovered wallet");
	} else {
		println!("Please enter a password for your new wallet");
	}

	let password = match g_args.password.clone() {
		Some(p) => p,
		None => prompt_password_confirm(),
	};

	Ok(command::InitArgs {
		list_length: list_length,
		password: password,
		config: config.clone(),
		recovery_phrase: recovery_phrase,
		restore: false,
	})
}

pub fn parse_recover_args(
	g_args: &command::GlobalArgs,
) -> Result<command::RecoverArgs, ParseError>
where
{
	let passphrase = prompt_password(&g_args.password);
	Ok(command::RecoverArgs {
		passphrase: passphrase,
	})
}

pub fn parse_listen_args(
	config: &mut WalletConfig,
	tor_config: &mut TorConfig,
	args: &ArgMatches,
) -> Result<command::ListenArgs, ParseError> {
	if let Some(port) = args.value_of("port") {
		config.api_listen_port = port.parse().unwrap();
	}
	if let Some(port) = args.value_of("libp2p_port") {
		config.libp2p_listen_port = Some(port.parse().unwrap());
	}

	let method = parse_required(args, "method")?;
	if args.is_present("no_tor") {
		tor_config.use_tor_listener = false;
	}
	Ok(command::ListenArgs {
		method: method.to_owned(),
	})
}

pub fn parse_owner_api_args(
	config: &mut WalletConfig,
	args: &ArgMatches,
) -> Result<(), ParseError> {
	if let Some(port) = args.value_of("port") {
		config.owner_api_listen_port = Some(port.parse().unwrap());
	}
	if args.is_present("run_foreign") {
		config.owner_api_include_foreign = Some(true);
	}
	Ok(())
}

pub fn parse_account_args(account_args: &ArgMatches) -> Result<command::AccountArgs, ParseError> {
	let create = match account_args.value_of("create") {
		None => None,
		Some(s) => Some(s.to_owned()),
	};
	Ok(command::AccountArgs { create: create })
}

pub fn parse_send_args(args: &ArgMatches) -> Result<command::SendArgs, ParseError> {
	// amount
	let amount = parse_required(args, "amount")?;
	let amount = core::core::amount_from_hr_string(amount);
	let amount = match amount {
		Ok(a) => a,
		Err(e) => {
			let msg = format!(
				"Could not parse amount as a number with optional decimal point. e={}",
				e
			);
			return Err(ParseError::ArgumentError(msg));
		}
	};

	// message
	let message = match args.is_present("message") {
		true => Some(args.value_of("message").unwrap().to_owned()),
		false => None,
	};

	// minimum_confirmations
	let min_c = parse_required(args, "minimum_confirmations")?;
	let min_c = parse_u64(min_c, "minimum_confirmations")?;

	// selection_strategy
	let selection_strategy = parse_required(args, "selection_strategy")?;

	// estimate_selection_strategies
	let estimate_selection_strategies = args.is_present("estimate_selection_strategies");

	let late_lock = args.is_present("late_lock");

	// method
	let method = parse_required(args, "method")?;
	let address = {
		if method == "file" && args.is_present("proof") {
			Some("file_proof".to_owned())
		} else if method == "file" {
			Some("file".to_owned())
		} else {
			None.to_owned()
		}
	};

	// dest
	let dest = {
		if method == "self" {
			match args.value_of("dest") {
				Some(d) => d,
				None => "default",
			}
		} else {
			if !estimate_selection_strategies && method != "slatepack" {
				parse_required(args, "dest")?
			} else {
				""
			}
		}
	};

	let apisecret = args.value_of("apisecret").map(|s| String::from(s));

	if !estimate_selection_strategies
		&& method == "http"
		&& !dest.starts_with("http://")
		&& !dest.starts_with("https://")
		&& is_tor_address(&dest).is_err()
	{
		let msg = format!(
			"HTTP Destination should start with http://: or https://: {}",
			dest,
		);
		return Err(ParseError::ArgumentError(msg));
	}

	// change_outputs
	let change_outputs = parse_required(args, "change_outputs")?;
	let change_outputs = parse_u64(change_outputs, "change_outputs")? as usize;

	// fluff
	let fluff = args.is_present("fluff");

	// ttl_blocks
	let ttl_blocks = parse_u64_or_none(args.value_of("ttl_blocks"));

	// max_outputs
	let max_outputs = 500;

	// target slate version to create/send
	let target_slate_version = {
		match args.is_present("slate_version") {
			true => {
				let v = parse_required(args, "slate_version")?;
				Some(parse_u64(v, "slate_version")? as u16)
			}
			false => None,
		}
	};

	let payment_proof_address = {
		match args.is_present("request_payment_proof")
			|| (args.is_present("proof") && method != "file")
		{
			true => {
				// if the destination address is a TOR address, we don't need the address
				// separately

				let proof_dest = proofaddress::address_to_pubkey(dest.to_string());
				match ProvableAddress::from_str(&proof_dest) {
					Ok(a) => Some(a),
					Err(_) => {
						let addr = parse_required(args, "proof_address")?;
						match ProvableAddress::from_str(&proofaddress::address_to_pubkey(
							addr.to_string(),
						)) {
							Ok(a) => Some(a),
							Err(e) => {
								let msg = format!("Invalid proof address: {:?}", e);
								return Err(ParseError::ArgumentError(msg));
							}
						}
					}
				}
			}
			false => None,
		}
	};

	let minimum_confirmations_change_outputs_is_present =
		args.occurrences_of("minimum_confirmations_change_outputs") != 0;
	let minimum_confirmations_change_outputs =
		parse_required(args, "minimum_confirmations_change_outputs")?;
	let minimum_confirmations_change_outputs = parse_u64(
		minimum_confirmations_change_outputs,
		"minimum_confirmations_change_outputs",
	)?;
	let exclude_change_outputs = args.is_present("exclude_change_outputs");

	let outputs = match args.is_present("outputs") {
		true => Some(
			args.value_of("outputs")
				.unwrap()
				.split(",")
				.map(|s| s.to_string())
				.collect::<Vec<String>>(),
		),
		false => None,
	};

	let slatepack_recipient: Option<ProvableAddress> = match args.value_of("slatepack_recipient") {
		Some(s) => {
			let addr = ProvableAddress::from_str(s).map_err(|e| {
				ParseError::ArgumentError(format!("Unable to parse slatepack_recipient, {}", e))
			})?;

			if addr.tor_public_key().is_err() {
				return Err(ParseError::ArgumentError(
					"Expecting tor PK address as a slatepack recipient value".to_string(),
				)
				.into());
			}
			Some(addr)
		}
		None => None,
	};

	let min_fee = match args.value_of("min_fee") {
		Some(min_fee) => match core::core::amount_from_hr_string(min_fee) {
			Ok(min_fee) => Some(min_fee),
			Err(e) => {
				return Err(ParseError::ArgumentError(format!(
					"Could not parse minimal fee as a number, {}",
					e
				)))
			}
		},
		None => None,
	};

	if minimum_confirmations_change_outputs_is_present && !exclude_change_outputs {
		Err(ArgumentError("minimum_confirmations_change_outputs may only be specified if exclude_change_outputs is set".to_string()))
	} else {
		Ok(command::SendArgs {
			amount: amount,
			message: message,
			minimum_confirmations: min_c,
			selection_strategy: selection_strategy.to_owned(),
			estimate_selection_strategies,
			method: method.to_owned(),
			dest: dest.to_owned(),
			apisecret: apisecret,
			change_outputs: change_outputs,
			fluff: fluff,
			max_outputs: max_outputs,
			payment_proof_address,
			ttl_blocks,
			target_slate_version: target_slate_version,
			exclude_change_outputs: exclude_change_outputs,
			minimum_confirmations_change_outputs: minimum_confirmations_change_outputs,
			address: address,
			outputs,
			slatepack_recipient,
			late_lock,
			min_fee,
		})
	}
}

pub fn parse_receive_unpack_args(args: &ArgMatches) -> Result<command::ReceiveArgs, ParseError> {
	// input file
	let input_file = match args.is_present("file") {
		true => {
			let file = args.value_of("file").unwrap().to_owned();
			// validate input
			if !Path::new(&file).is_file() {
				let msg = format!("File {} not found.", &file);
				return Err(ParseError::ArgumentError(msg));
			}
			Some(file)
		}
		false => None,
	};

	Ok(command::ReceiveArgs {
		input_file,
		input_slatepack_message: args.value_of("content").map(|s| s.to_string()),
		message: args.value_of("message").map(|s| s.to_string()),
		outfile: args.value_of("outfile").map(|s| s.to_string()),
	})
}

pub fn parse_finalize_args(args: &ArgMatches) -> Result<command::FinalizeArgs, ParseError> {
	// input file
	let input_file = match args.is_present("file") {
		true => {
			let file = args.value_of("file").unwrap().to_owned();
			// validate input
			if !Path::new(&file).is_file() {
				let msg = format!("File {} not found.", &file);
				return Err(ParseError::ArgumentError(msg));
			}
			Some(file)
		}
		false => None,
	};

	Ok(command::FinalizeArgs {
		input_file,
		input_slatepack_message: args.value_of("content").map(|s| s.to_string()),
		fluff: args.is_present("fluff"),
		nopost: args.is_present("nopost"),
		dest: args.value_of("dest").map(|s| s.to_string()),
	})
}

pub fn parse_issue_invoice_args(
	args: &ArgMatches,
) -> Result<command::IssueInvoiceArgs, ParseError> {
	let amount = parse_required(args, "amount")?;
	let amount = core::core::amount_from_hr_string(amount);
	let amount = match amount {
		Ok(a) => a,
		Err(e) => {
			let msg = format!(
				"Could not parse amount as a number with optional decimal point. e={}",
				e
			);
			return Err(ParseError::ArgumentError(msg));
		}
	};
	// message
	let message = match args.is_present("message") {
		true => Some(args.value_of("message").unwrap().to_owned()),
		false => None,
	};
	// target slate version to create
	let target_slate_version = {
		match args.is_present("slate_version") {
			true => {
				let v = parse_required(args, "slate_version")?;
				Some(parse_u64(v, "slate_version")? as u16)
			}
			false => None,
		}
	};

	let slatepack_recipient: Option<ProvableAddress> = match args.value_of("slatepack_recipient") {
		Some(s) => {
			let addr = ProvableAddress::from_str(s).map_err(|e| {
				ParseError::ArgumentError(format!("Unable to parse slatepack_recipient, {}", e))
			})?;

			if addr.tor_public_key().is_err() {
				return Err(ParseError::ArgumentError(
					"Expecting tor PK address as a slatepack recipient value".to_string(),
				)
				.into());
			}
			Some(addr)
		}
		None => None,
	};

	// dest (output file)
	let dest = parse_required(args, "dest")?;
	Ok(command::IssueInvoiceArgs {
		dest: dest.into(),
		issue_args: IssueInvoiceTxArgs {
			dest_acct_name: None,
			address: Some(String::from(dest)),
			amount,
			message,
			target_slate_version,
			slatepack_recipient,
		},
	})
}

pub fn parse_process_invoice_args(
	args: &ArgMatches,
	prompt: bool,
	slatepack_secret: &DalekSecretKey,
) -> Result<command::ProcessInvoiceArgs, ParseError> {
	// TODO: display and prompt for confirmation of what we're doing
	// message
	let message = match args.is_present("message") {
		true => Some(args.value_of("message").unwrap().to_owned()),
		false => None,
	};

	// minimum_confirmations
	let min_c = parse_required(args, "minimum_confirmations")?;
	let min_c = parse_u64(min_c, "minimum_confirmations")?;

	// selection_strategy
	let selection_strategy = parse_required(args, "selection_strategy")?;

	// estimate_selection_strategies
	let estimate_selection_strategies = args.is_present("estimate_selection_strategies");

	// method
	let method = parse_required(args, "method")?;

	// dest
	let dest = {
		if method == "self" {
			match args.value_of("dest") {
				Some(d) => d,
				None => "default",
			}
		} else {
			if !estimate_selection_strategies {
				parse_required(args, "dest")?
			} else {
				""
			}
		}
	};
	if !estimate_selection_strategies
		&& method == "http"
		&& !dest.starts_with("http://")
		&& !dest.starts_with("https://")
	{
		let msg = format!(
			"HTTP Destination should start with http://: or https://: {}",
			dest,
		);
		return Err(ParseError::ArgumentError(msg));
	}

	// ttl_blocks
	let ttl_blocks = parse_u64_or_none(args.value_of("ttl_blocks"));

	// max_outputs
	let max_outputs = 500;

	// file input only
	let tx_file = parse_required(args, "input")?;

	if prompt {
		// Now we need to prompt the user whether they want to do this,
		// which requires reading the slate
		let slate =
			match PathToSlateGetter::build_form_path((&tx_file).into()).get_tx(slatepack_secret) {
				Ok(s) => s,
				Err(e) => return Err(ParseError::ArgumentError(format!("{}", e))),
			};
		let slate = slate
			.to_slate()
			.map_err(|e| ParseError::ArgumentError(format!("Unable to read the slate, {}", e)))?
			.0;

		prompt_pay_invoice(&slate, method, dest)?;
	}

	Ok(command::ProcessInvoiceArgs {
		message: message,
		minimum_confirmations: min_c,
		selection_strategy: selection_strategy.to_owned(),
		estimate_selection_strategies,
		method: method.to_owned(),
		dest: dest.to_owned(),
		max_outputs: max_outputs,
		input: tx_file.to_owned(),
		ttl_blocks,
	})
}

pub fn parse_info_args(args: &ArgMatches) -> Result<command::InfoArgs, ParseError> {
	// minimum_confirmations
	let mc = parse_required(args, "minimum_confirmations")?;
	let mc = parse_u64(mc, "minimum_confirmations")?;
	Ok(command::InfoArgs {
		minimum_confirmations: mc,
	})
}

pub fn parse_check_args(args: &ArgMatches) -> Result<command::CheckArgs, ParseError> {
	let delete_unconfirmed = args.is_present("delete_unconfirmed");
	let start_height = parse_u64_or_none(args.value_of("start_height"));

	let backwards_from_tip = parse_u64_or_none(args.value_of("backwards_from_tip"));
	if backwards_from_tip.is_some() && start_height.is_some() {
		let msg = format!("backwards_from tip and start_height cannot both be present");
		return Err(ParseError::ArgumentError(msg));
	}
	Ok(command::CheckArgs {
		start_height,
		backwards_from_tip,
		delete_unconfirmed,
	})
}

pub fn parse_txs_args(args: &ArgMatches) -> Result<command::TxsArgs, ParseError> {
	let tx_id = match args.value_of("id") {
		None => None,
		Some(tx) => Some(parse_u64(tx, "id")? as u32),
	};
	let tx_slate_id = match args.value_of("txid") {
		None => None,
		Some(tx) => match tx.parse() {
			Ok(t) => Some(t),
			Err(e) => {
				let msg = format!("Could not parse txid parameter. e={}", e);
				return Err(ParseError::ArgumentError(msg));
			}
		},
	};
	if tx_id.is_some() && tx_slate_id.is_some() {
		let msg = format!("At most one of 'id' (-i) or 'txid' (-t) may be provided.");
		return Err(ParseError::ArgumentError(msg));
	}
	Ok(command::TxsArgs {
		id: tx_id,
		tx_slate_id: tx_slate_id,
	})
}

pub fn parse_post_args(args: &ArgMatches) -> Result<command::PostArgs, ParseError> {
	let tx_file = parse_required(args, "input")?;
	let fluff = args.is_present("fluff");

	Ok(command::PostArgs {
		input: tx_file.to_owned(),
		fluff: fluff,
	})
}

pub fn parse_submit_args(args: &ArgMatches) -> Result<command::SubmitArgs, ParseError> {
	// input
	let tx_file = parse_required(args, "input")?;

	// validate input
	if !Path::new(&tx_file).is_file() {
		return Err(ParseError::ArgumentError(format!(
			"File {} not found.",
			&tx_file
		)));
	}

	// check fluff flag
	let fluff = args.is_present("fluff");

	Ok(command::SubmitArgs {
		input: tx_file.to_owned(),
		fluff: fluff,
	})
}

pub fn parse_repost_args(args: &ArgMatches) -> Result<command::RepostArgs, ParseError> {
	let tx_id = match args.value_of("id") {
		None => None,
		Some(tx) => Some(parse_u64(tx, "id")? as u32),
	};

	let fluff = args.is_present("fluff");
	let dump_file = match args.value_of("dumpfile") {
		None => None,
		Some(d) => Some(d.to_owned()),
	};

	Ok(command::RepostArgs {
		id: tx_id.unwrap(),
		dump_file: dump_file,
		fluff: fluff,
	})
}

pub fn parse_cancel_args(args: &ArgMatches) -> Result<command::CancelArgs, ParseError> {
	let mut tx_id_string = "";
	let tx_id = match args.value_of("id") {
		None => None,
		Some(tx) => Some(parse_u64(tx, "id")? as u32),
	};
	let tx_slate_id = match args.value_of("txid") {
		None => None,
		Some(tx) => match tx.parse() {
			Ok(t) => {
				tx_id_string = tx;
				Some(t)
			}
			Err(e) => {
				let msg = format!("Could not parse txid parameter. e={}", e);
				return Err(ParseError::ArgumentError(msg));
			}
		},
	};
	if (tx_id.is_none() && tx_slate_id.is_none()) || (tx_id.is_some() && tx_slate_id.is_some()) {
		let msg = format!("'id' (-i) or 'txid' (-t) argument is required.");
		return Err(ParseError::ArgumentError(msg));
	}
	Ok(command::CancelArgs {
		tx_id: tx_id,
		tx_slate_id: tx_slate_id,
		tx_id_string: tx_id_string.to_owned(),
	})
}

pub fn parse_export_proof_args(args: &ArgMatches) -> Result<command::ProofExportArgs, ParseError> {
	let output_file = parse_required(args, "output")?;
	let tx_id = match args.value_of("id") {
		None => None,
		Some(tx) => Some(parse_u64(tx, "id")? as u32),
	};
	let tx_slate_id = match args.value_of("txid") {
		None => None,
		Some(tx) => match tx.parse() {
			Ok(t) => Some(t),
			Err(e) => {
				let msg = format!("Could not parse txid parameter. e={}", e);
				return Err(ParseError::ArgumentError(msg));
			}
		},
	};
	if tx_id.is_some() && tx_slate_id.is_some() {
		let msg = format!("At most one of 'id' (-i) or 'txid' (-t) may be provided.");
		return Err(ParseError::ArgumentError(msg));
	}
	if tx_id.is_none() && tx_slate_id.is_none() {
		let msg = format!("Either 'id' (-i) or 'txid' (-t) must be provided.");
		return Err(ParseError::ArgumentError(msg));
	}
	Ok(command::ProofExportArgs {
		output_file: output_file.to_owned(),
		id: tx_id,
		tx_slate_id: tx_slate_id,
	})
}

pub fn parse_verify_proof_args(args: &ArgMatches) -> Result<command::ProofVerifyArgs, ParseError> {
	let input_file = parse_required(args, "input")?;
	Ok(command::ProofVerifyArgs {
		input_file: input_file.to_owned(),
	})
}

pub fn parse_swap_start_args(args: &ArgMatches) -> Result<SwapStartArgs, ParseError> {
	let mwc_amount = parse_required(args, "mwc_amount")?;
	let mwc_amount = core::core::amount_from_hr_string(mwc_amount);
	let mwc_amount = match mwc_amount {
		Ok(a) => a,
		Err(e) => {
			let msg = format!(
				"Could not parse MWC amount as a number with optional decimal point. e={}",
				e
			);
			return Err(ParseError::ArgumentError(msg));
		}
	};

	let min_c = parse_required(args, "minimum_confirmations")?;
	let min_c = parse_u64(min_c, "minimum_confirmations")?;

	let secondary_currency = parse_required(args, "secondary_currency")?;
	let secondary_currency = secondary_currency.to_lowercase();
	match secondary_currency.as_str() {
		"btc" | "bch" | "ltc" | "zcash" | "dash" | "doge" => (),
		_ => {
			return Err(ParseError::ArgumentError(format!(
				"{} is not on the supported currency list.",
				secondary_currency
			)))
		}
	}

	let btc_amount = parse_required(args, "secondary_amount")?;
	let btc_address = parse_required(args, "secondary_address")?;

	let who_lock_first = parse_required(args, "who_lock_first")?.to_lowercase();
	if !(who_lock_first == "buyer" || who_lock_first == "seller") {
		return Err(ParseError::ArgumentError(format!(
			"Expected who_lock_first values are 'buyer' or 'seller'. Get {}",
			who_lock_first
		)));
	}

	let mwc_lock = parse_required(args, "mwc_confirmations")?;
	let mwc_lock = parse_u64(mwc_lock, "mwc_confirmations")?;

	let btc_lock = parse_required(args, "secondary_confirmations")?;
	let btc_lock = parse_u64(btc_lock, "secondary_confirmations")?;

	let message_exchange_time = parse_required(args, "message_exchange_time")?;
	let message_exchange_time = parse_u64(message_exchange_time, "message_exchange_time")?;

	let redeem_time = parse_required(args, "redeem_time")?;
	let redeem_time = parse_u64(redeem_time, "redeem_time")?;

	let method = parse_required(args, "method")?;
	let destination = parse_required(args, "dest")?;

	let electrum_node_uri1 = args
		.value_of("electrum_uri1")
		.map(|s| String::from(s))
		.filter(|s| !s.is_empty());
	let electrum_node_uri2 = args
		.value_of("electrum_uri2")
		.map(|s| String::from(s))
		.filter(|s| !s.is_empty());

	let secondary_fee = match args.value_of("secondary_fee") {
		Some(fee_str) => Some(fee_str.parse::<f32>().map_err(|e| {
			ParseError::ArgumentError(format!("Invalid secondary_fee value, {}", e))
		})?),
		None => None,
	};

	let dry_run = args.is_present("dry_run");

	Ok(SwapStartArgs {
		mwc_amount,
		outputs: args
			.value_of("outputs")
			.map(|s| s.split(",").map(|s| s.to_string()).collect::<Vec<String>>()),
		secondary_currency: secondary_currency.to_string(),
		secondary_amount: btc_amount.to_string(),
		secondary_redeem_address: btc_address.to_string(),
		secondary_fee,
		seller_lock_first: who_lock_first == "seller",
		minimum_confirmations: Some(min_c),
		mwc_confirmations: mwc_lock,
		secondary_confirmations: btc_lock,
		message_exchange_time_sec: message_exchange_time * 60,
		redeem_time_sec: redeem_time * 60,
		buyer_communication_method: method.to_string(),
		buyer_communication_address: destination.to_string(),
		electrum_node_uri1,
		electrum_node_uri2,
		dry_run,
		tag: args.value_of("tag").map(|s| s.to_string()),
	})
}

pub fn parse_swap_args(args: &ArgMatches) -> Result<command::SwapArgs, ParseError> {
	let swap_id = args.value_of("swap_id").map(|s| String::from(s));
	let adjust = args
		.value_of("adjust")
		.map(|s| s.split(",").map(|s| String::from(s)).collect())
		.unwrap_or(vec![]);
	let method = args.value_of("method").map(|s| String::from(s));
	let mut destination = args.value_of("dest").map(|s| String::from(s));
	let apisecret = args.value_of("apisecret").map(|s| String::from(s));
	let secondary_fee = match args.value_of("secondary_fee") {
		Some(s) => Some(parse_f32(s, "secondary_fee")?),
		None => None,
	};
	let message_file_name = args.value_of("message_file_name").map(|s| String::from(s));
	let buyer_refund_address = args
		.value_of("buyer_refund_address")
		.map(|s| String::from(s));
	let secondary_address = args.value_of("secondary_address").map(|s| String::from(s));
	let start_listener = args.is_present("start_listener");

	let subcommand = if args.is_present("list") {
		if args.is_present("check") {
			command::SwapSubcommand::ListAndCheck
		} else {
			command::SwapSubcommand::List
		}
	} else if args.is_present("remove") {
		command::SwapSubcommand::Delete
	} else if args.is_present("check") {
		command::SwapSubcommand::Check
	} else if args.is_present("process") {
		command::SwapSubcommand::Process
	} else if args.is_present("dump") {
		command::SwapSubcommand::Dump
	} else if args.is_present("trade_export") {
		destination = args.value_of("trade_export").map(|s| String::from(s));
		command::SwapSubcommand::TradeExport
	} else if args.is_present("trade_import") {
		destination = args.value_of("trade_import").map(|s| String::from(s));
		command::SwapSubcommand::TradeImport
	} else if !adjust.is_empty() {
		command::SwapSubcommand::Adjust
	} else if args.is_present("autoswap") {
		command::SwapSubcommand::Autoswap
	} else if args.is_present("stop_auto_swap") {
		command::SwapSubcommand::StopAllAutoSwap
	} else {
		return Err(ParseError::ArgumentError(format!(
			"Please define some action to do"
		)));
	};

	let electrum_node_uri1 = args.value_of("electrum_uri1").map(|s| String::from(s));
	let electrum_node_uri2 = args.value_of("electrum_uri2").map(|s| String::from(s));

	Ok(command::SwapArgs {
		subcommand,
		swap_id,
		adjust,
		method,
		destination,
		apisecret,
		secondary_fee,
		message_file_name,
		buyer_refund_address,
		start_listener,
		secondary_address,
		json_format: false,
		electrum_node_uri1,
		electrum_node_uri2,
		wait_for_backup1: false, // waiting is a primary usage for qt wallet. We are not documented that properly to make available for all users.
		tag: args.value_of("tag").map(|s| String::from(s)),
	})
}

pub fn parse_integrity_args(args: &ArgMatches) -> Result<command::IntegrityArgs, ParseError> {
	let mut fee = vec![];
	let subcommand = if args.is_present("check") {
		command::IntegritySubcommand::Check
	} else if args.is_present("fee") {
		let fee_str = parse_required(args, "fee")?.split(",");
		for fs in fee_str {
			let fee_amount = core::core::amount_from_hr_string(fs).map_err(|e| {
				ParseError::ArgumentError(format!("Unable to parse create fee amount, {}", e))
			})?;
			fee.push(fee_amount);
		}
		command::IntegritySubcommand::Create
	} else if args.is_present("withdraw") {
		command::IntegritySubcommand::Withdraw
	} else {
		return Err(ParseError::ArgumentError(
			"Expected check, create or withdraw parameter".to_string(),
		));
	};

	let reserve = match args.value_of("reserve") {
		Some(str) => Some(core::core::amount_from_hr_string(str).map_err(|e| {
			ParseError::ArgumentError(format!("Unable to parse reserve MWC value, {}", e))
		})?),
		None => None,
	};
	let account = args.value_of("account").map(|s| String::from(s));

	Ok(command::IntegrityArgs {
		subcommand,
		account,
		reserve,
		fee,
		json: args.is_present("json"),
	})
}

pub fn parse_messaging_args(args: &ArgMatches) -> Result<command::MessagingArgs, ParseError> {
	let fee = match args.value_of("fee") {
		Some(s) => Some(core::core::amount_from_hr_string(s).map_err(|e| {
			ParseError::ArgumentError(format!("Unable to parse create fee amount, {}", e))
		})?),
		None => None,
	};

	let fee_uuid = match args.value_of("fee_uuid") {
		Some(s) => Some(Uuid::parse_str(s).map_err(|e| {
			ParseError::ArgumentError(format!("Unable to parse fee_uuid value, {}", e))
		})?),
		None => None,
	};

	let publish_interval = match args.value_of("publish_interval") {
		Some(s) => Some(s.parse::<u32>().map_err(|e| {
			ParseError::ArgumentError(format!("Unable to parse interval value, {}", e))
		})?),
		None => None,
	};

	Ok(command::MessagingArgs {
		show_status: args.is_present("status"),
		add_topic: args.value_of("add_topic").map(|s| String::from(s)),
		fee,
		fee_uuid,
		remove_topic: args.value_of("remove_topic").map(|s| String::from(s)),
		publish_message: args.value_of("publish_message").map(|s| String::from(s)),
		publish_topic: args.value_of("publish_topic").map(|s| String::from(s)),
		publish_interval,
		withdraw_message_id: args.value_of("message_uuid").map(|s| String::from(s)),
		receive_messages: args.value_of("delete_messages").map(|s| s == "yes"),
		check_integrity_expiration: args.is_present("check_integrity"),
		check_integrity_retain: args.is_present("check_integrity_retain"),
		json: args.is_present("json"),
	})
}

pub fn parse_send_marketplace_message(
	args: &ArgMatches,
) -> Result<command::SendMarketplaceMessageArgs, ParseError> {
	Ok(command::SendMarketplaceMessageArgs {
		command: parse_required(args, "command")?.to_string(),
		offer_id: parse_required(args, "offer_id")?.to_string(),
		tor_address: parse_required(args, "tor_address")?.to_string(),
	})
}

pub fn wallet_command<C, F>(
	wallet_args: &ArgMatches,
	mut wallet_config: WalletConfig,
	tor_config: Option<TorConfig>,
	mqs_config: Option<MQSConfig>,
	mut node_client: C,
	test_mode: bool,
	wallet_inst_cb: F,
) -> Result<String, Error>
where
	C: NodeClient + 'static + Clone,
	F: FnOnce(
		Arc<
			Mutex<
				Box<
					dyn WalletInst<
						'static,
						DefaultLCProvider<'static, C, keychain::ExtKeychain>,
						C,
						keychain::ExtKeychain,
					>,
				>,
			>,
		>,
	),
{
	if let Some(t) = wallet_config.chain_type.clone() {
		core::global::set_local_chain_type(t);
	}

	if wallet_args.is_present("external") {
		wallet_config.api_listen_interface = "0.0.0.0".to_string();
	}

	if let Some(dir) = wallet_args.value_of("top_level_dir") {
		wallet_config.data_file_dir = dir.to_string().clone();
	}

	if let Some(sa) = wallet_args.value_of("api_server_address") {
		wallet_config.check_node_api_http_addr = sa.to_string().clone();
	}

	let global_wallet_args = arg_parse!(parse_global_args(&wallet_config, &wallet_args));

	//parse the nodes address and put them in a vec
	let node_list = parse_node_address_string(wallet_config.check_node_api_http_addr.clone());

	node_client.set_node_url(node_list);
	node_client.set_node_api_secret(global_wallet_args.node_api_secret.clone());
	let node_client_index = node_client.get_node_index();

	// legacy hack to avoid the need for changes in existing grin-wallet.toml files
	// remove `wallet_data` from end of path as
	// new lifecycle provider assumes grin_wallet.toml is in root of data directory
	let mut top_level_wallet_dir = PathBuf::from(wallet_config.clone().data_file_dir);
	if top_level_wallet_dir.ends_with(GRIN_WALLET_DIR) {
		top_level_wallet_dir.pop();
		wallet_config.data_file_dir = top_level_wallet_dir.to_str().unwrap().into();
	}

	// for backwards compatibility: If tor config doesn't exist in the file, assume
	// the top level directory for data
	let tor_config = match tor_config {
		Some(tc) => tc,
		None => {
			let mut tc = TorConfig::default();
			tc.send_config_dir = wallet_config.data_file_dir.clone();
			tc
		}
	};

	let mqs_config = match mqs_config {
		Some(mqs) => mqs,
		None => {
			let mqs = MQSConfig::default();
			mqs
		}
	};

	// Instantiate wallet (doesn't open the wallet)
	let wallet =
		inst_wallet::<DefaultLCProvider<C, keychain::ExtKeychain>, C, keychain::ExtKeychain>(
			wallet_config.clone(),
			node_client.clone(),
		)
		.unwrap_or_else(|e| {
			println!("{}", e);
			std::process::exit(1);
		});

	{
		let mut wallet_lock = wallet.lock();
		let lc = wallet_lock.lc_provider().unwrap();
		let _ = lc.set_top_level_directory(&wallet_config.data_file_dir);
	}

	// provide wallet instance back to the caller (handy for testing with
	// local wallet proxy, etc)
	wallet_inst_cb(wallet.clone());

	// don't open wallet for certain lifecycle commands
	let mut open_wallet = true;
	match wallet_args.subcommand() {
		("init", Some(_)) => open_wallet = false,
		("recover", _) => open_wallet = false,
		("cli", _) => open_wallet = false,
		("owner_api", _) => {
			// If wallet exists, open it. Otherwise, that's fine too.
			let mut wallet_lock = wallet.lock();
			let lc = wallet_lock.lc_provider().unwrap();
			open_wallet = lc.wallet_exists(None, wallet_config.wallet_data_dir.as_deref())?;
		}
		_ => {}
	}

	let keychain_mask = match open_wallet {
		true => {
			let mut wallet_lock = wallet.lock();
			let lc = wallet_lock.lc_provider().unwrap();
			let mask = lc.open_wallet(
				None,
				prompt_password(&global_wallet_args.password),
				false,
				false,
				wallet_config.wallet_data_dir.as_deref(),
			)?;

			let wallet_inst = lc.wallet_inst()?;

			grin_wallet_libwallet::swap::trades::init_swap_trade_backend(
				wallet_inst.get_data_file_dir(),
				&wallet_config.swap_electrumx_addr,
			);

			//read or save the node index(the good node)
			{
				let mut batch = wallet_inst.batch(mask.as_ref())?;
				let index = batch.get_last_working_node_index()?;
				if index == 0 {
					let _ = batch.save_last_working_node_index(node_client_index + 1); //index stored in db start from 1. need to offset by +1
				} else {
					node_client.set_node_index(index - 1); //index stored in db start from 1. need to offset by -1
				}
				batch.commit()?;
			}

			if let Some(account) = wallet_args.value_of("account") {
				wallet_inst.set_parent_key_id_by_name(account)?;
			}
			mask
		}
		false => None,
	};

	let res = match wallet_args.subcommand() {
		("cli", Some(_)) => command_loop(
			wallet,
			keychain_mask,
			&wallet_config,
			&tor_config,
			&mqs_config,
			&global_wallet_args,
			test_mode,
		),
		_ => {
			let mut owner_api = Owner::new(wallet, None, Some(tor_config.clone()));
			parse_and_execute(
				&mut owner_api,
				keychain_mask,
				&wallet_config,
				&tor_config,
				&mqs_config,
				&global_wallet_args,
				&wallet_args,
				test_mode,
				false,
			)
		}
	};

	if let Err(e) = res {
		Err(e)
	} else {
		Ok(wallet_args.subcommand().0.to_owned())
	}
}

pub fn parse_and_execute<L, C, K>(
	owner_api: &mut Owner<L, C, K>,
	keychain_mask: Option<SecretKey>,
	wallet_config: &WalletConfig,
	tor_config: &TorConfig,
	mqs_config: &MQSConfig,
	global_wallet_args: &command::GlobalArgs,
	wallet_args: &ArgMatches,
	test_mode: bool,
	cli_mode: bool,
) -> Result<(), Error>
where
	DefaultWalletImpl<'static, C>: WalletInst<'static, L, C, K>,
	L: WalletLCProvider<'static, C, K> + 'static,
	C: NodeClient + 'static,
	K: keychain::Keychain + 'static,
{
	let km = (&keychain_mask).as_ref();
	match wallet_args.subcommand() {
		("init", Some(args)) => {
			let a = arg_parse!(parse_init_args(
				owner_api.wallet_inst.clone(),
				wallet_config,
				global_wallet_args,
				&args,
				test_mode,
			));
			command::init(
				owner_api,
				&global_wallet_args,
				a,
				wallet_config.wallet_data_dir.as_deref(),
			)
		}
		("recover", Some(_)) => {
			let a = arg_parse!(parse_recover_args(&global_wallet_args,));
			command::recover(owner_api, a, wallet_config.wallet_data_dir.as_deref())
		}
		("listen", Some(args)) => {
			let mut c = wallet_config.clone();
			let mut t = tor_config.clone();
			let m = mqs_config.clone();
			let a = arg_parse!(parse_listen_args(&mut c, &mut t, &args)); //TODO: be able to pass in mqs domain and port.
			command::listen(
				owner_api,
				Arc::new(Mutex::new(keychain_mask)),
				&c,
				&t,
				&m,
				&a,
				&global_wallet_args.clone(),
				cli_mode,
			)
		}
		("owner_api", Some(args)) => {
			let mut c = wallet_config.clone();
			let mut g = global_wallet_args.clone();
			g.tls_conf = None;
			arg_parse!(parse_owner_api_args(&mut c, &args));
			command::owner_api(owner_api, keychain_mask, &c, &tor_config, &mqs_config, &g)
		}
		("web", Some(_)) => command::owner_api(
			owner_api,
			keychain_mask,
			wallet_config,
			tor_config,
			mqs_config,
			global_wallet_args,
		),
		("account", Some(args)) => {
			let a = arg_parse!(parse_account_args(&args));
			command::account(owner_api, km, a)
		}
		("send", Some(args)) => {
			let a = arg_parse!(parse_send_args(&args));
			command::send(
				owner_api,
				&wallet_config,
				km,
				wallet_config.api_listen_addr(),
				global_wallet_args.tls_conf.clone(),
				Some(tor_config.clone()),
				Some(mqs_config.clone()),
				a,
				wallet_config.dark_background_color_scheme.unwrap_or(true),
			)
		}
		("unpack", Some(args)) => {
			let a = arg_parse!(parse_receive_unpack_args(&args));
			command::unpack(owner_api, km, a)
		}
		("receive", Some(args)) => {
			let a = arg_parse!(parse_receive_unpack_args(&args));
			command::receive(owner_api, km, &global_wallet_args, a)
		}
		("finalize", Some(args)) => {
			let a = arg_parse!(parse_finalize_args(&args));
			command::finalize(owner_api, km, a, false)
		}
		("finalize_invoice", Some(args)) => {
			let a = arg_parse!(parse_finalize_args(&args));
			command::finalize(owner_api, km, a, true)
		}
		("invoice", Some(args)) => {
			let a = arg_parse!(parse_issue_invoice_args(&args));
			command::issue_invoice_tx(owner_api, km, a)
		}
		("pay", Some(args)) => {
			let slatepack_secret = {
				let mut w_lock = owner_api.wallet_inst.lock();
				let w = w_lock.lc_provider()?.wallet_inst()?;
				let keychain = w.keychain(km)?;
				let slatepack_secret =
					proofaddress::payment_proof_address_dalek_secret(&keychain, None)?;
				slatepack_secret
			};

			let a = arg_parse!(parse_process_invoice_args(
				&args,
				!test_mode,
				&slatepack_secret
			));
			command::process_invoice(
				owner_api,
				km,
				Some(tor_config.clone()),
				a,
				wallet_config.dark_background_color_scheme.unwrap_or(true),
			)
		}
		("info", Some(args)) => {
			let a = arg_parse!(parse_info_args(&args));
			command::info(
				owner_api,
				km,
				&global_wallet_args,
				a,
				wallet_config.dark_background_color_scheme.unwrap_or(true),
			)
		}
		("outputs", Some(_)) => command::outputs(
			owner_api,
			km,
			&global_wallet_args,
			wallet_config.dark_background_color_scheme.unwrap_or(true),
		),
		("txs", Some(args)) => {
			let a = arg_parse!(parse_txs_args(&args));
			command::txs(
				owner_api,
				km,
				&global_wallet_args,
				a,
				wallet_config.dark_background_color_scheme.unwrap_or(true),
			)
		}
		("post", Some(args)) => {
			let a = arg_parse!(parse_post_args(&args));
			command::post(owner_api, km, a)
		}
		// Submit is a synonim for 'post'. Since MWC intoduce it ealier, let's keep it
		("submit", Some(args)) => {
			let a = arg_parse!(parse_submit_args(&args));
			command::submit(owner_api, km, a)
		}
		("repost", Some(args)) => {
			let a = arg_parse!(parse_repost_args(&args));
			command::repost(owner_api, km, a)
		}
		("cancel", Some(args)) => {
			let a = arg_parse!(parse_cancel_args(&args));
			command::cancel(owner_api, km, a)
		}
		("export_proof", Some(args)) => {
			let a = arg_parse!(parse_export_proof_args(&args));
			command::proof_export(owner_api, km, a)
		}
		("verify_proof", Some(args)) => {
			let a = arg_parse!(parse_verify_proof_args(&args));
			command::proof_verify(owner_api, km, a)
		}
		("address", Some(_)) => command::address(owner_api, &global_wallet_args, km),
		("scan", Some(args)) => {
			let a = arg_parse!(parse_check_args(&args));
			command::scan(owner_api, km, a)
		}
		("dump-wallet-data", Some(args)) => command::dump_wallet_data(
			owner_api,
			km,
			args.value_of("file").map(|s| String::from(s)),
		),
		("open", Some(_)) => {
			// for CLI mode only, should be handled externally
			Ok(())
		}
		("close", Some(_)) => {
			// for CLI mode only, should be handled externally
			Ok(())
		}
		("swap_start", Some(args)) => {
			let a = arg_parse!(parse_swap_start_args(&args));
			command::swap_start(owner_api, km, &a)
		}
		("swap_create_from_offer", Some(args)) => {
			let mwc_amount = arg_parse!(parse_required(args, "file"));
			command::swap_create_from_offer(owner_api, km, mwc_amount.to_string())
		}
		("swap", Some(args)) => {
			let a = arg_parse!(parse_swap_args(&args));
			command::swap(
				owner_api.wallet_inst.clone(),
				km,
				wallet_config.api_listen_addr(),
				mqs_config.clone(),
				tor_config.clone(),
				global_wallet_args.tls_conf.clone(),
				a,
				cli_mode,
			)
		}
		("integrity", Some(args)) => {
			let a = arg_parse!(parse_integrity_args(&args));
			command::integrity(owner_api.wallet_inst.clone(), km, a)
		}
		("messaging", Some(args)) => {
			let a = arg_parse!(parse_messaging_args(&args));
			command::messaging(owner_api.wallet_inst.clone(), km, a)
		}
		("send_marketplace_message", Some(args)) => {
			let a = arg_parse!(parse_send_marketplace_message(&args));
			command::send_marketplace_message(owner_api.wallet_inst.clone(), km, tor_config, a)
		}
		(cmd, _) => {
			return Err(ErrorKind::ArgumentError(format!(
				"Unknown wallet command '{}', use 'mwc help wallet' for details",
				cmd
			))
			.into());
		}
	}
}
