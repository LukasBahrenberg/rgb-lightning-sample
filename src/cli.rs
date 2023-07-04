use std::io;
use std::io::Write;

use crate::request::{self, AppDataStruct};
pub use serde_json::{self, Map, Value};

pub(crate) async fn poll_for_user_input(shared_data: AppDataStruct) {
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
		println!("{response}");

		if response == "quitting" {
			break;
		}
	}
}
