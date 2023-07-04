use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use actix_web::{self, post, rt, web::Data, App, HttpResponse, HttpServer, Responder};
pub use serde_json::{self, Map, Value};

use crate::request::{self, AppDataStruct};

pub(crate) fn start_api(shared_data: AppDataStruct, rest_api_port: u16) -> std::io::Result<()> {
	let shared_data: Data<Mutex<AppDataStruct>> = Data::new(Mutex::new(shared_data));

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
async fn cliwrapper(request: String, shared_data: Data<Mutex<AppDataStruct>>) -> impl Responder {
	let shared_data: &mut AppDataStruct = &mut *shared_data.lock().unwrap();

	// Parse request
	println!("Raw request: {:?}", request);
	let parsed_request: CliCommands = serde_json::from_str(&request).unwrap();
	println!("Parsed request: {:?}", parsed_request);
	let line: String = parsed_request.commands;

	let response = request::handle_request(line, shared_data.clone()).await;

	HttpResponse::Ok().body(response)
}
