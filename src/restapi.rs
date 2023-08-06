use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use actix_web::{self, post, rt, web::Data, App, HttpResponse, HttpServer, Responder};
pub use serde_json::{self, Map, Value};

use crate::cli::{self, SharedData};

pub(crate) fn start_api(shared_data: SharedData, rest_api_port: u16) -> std::io::Result<()> {
	let shared_data: Data<Mutex<SharedData>> = Data::new(Mutex::new(shared_data));

	println!("Starting REST API on port {}", rest_api_port);

	rt::System::new().block_on(
		HttpServer::new(move || App::new().app_data(shared_data.clone()).service(cliwrapper))
			.bind(("0.0.0.0", rest_api_port))?
			.run(),
	)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CliCommands {
	pub commands: String,
}

// generalized API wrapper around the cli functions
#[post("/cliwrapper")]
async fn cliwrapper(request: String, shared_data: Data<Mutex<SharedData>>) -> impl Responder {
	let shared_data: &mut SharedData = &mut *shared_data.lock().unwrap();

	// Parse request
	println!("Raw request: {:?}", request);
	let parsed_request: CliCommands = serde_json::from_str(&request).unwrap();
	println!("Parsed request: {:?}", parsed_request);
	let line: String = parsed_request.commands;

	let response = cli::handle_command(line, shared_data.clone()).await.unwrap();

	HttpResponse::Ok().body(response)
}