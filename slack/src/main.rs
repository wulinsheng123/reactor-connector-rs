use std::env;
use std::time::Duration;
use std::net::SocketAddr;
use serde::{Serialize, Deserialize};
use serde_json::Value;
use lazy_static::lazy_static;
use openssl::rsa::{Rsa, Padding};
use reqwest::{Client, ClientBuilder, multipart};
use axum::{
	Router,
	routing::{get, post},
	extract::{Query, Json},
	response::{IntoResponse},
	http::{StatusCode},
	extract::{ContentLengthLimit, Multipart},
};

lazy_static! {
	static ref REACTOR_API_PREFIX: String = env::var("REACTOR_API_PREFIX").expect("Env variable REACTOR_API_PREFIX not set");
	static ref REACTOR_AUTH_TOKEN: String = env::var("REACTOR_AUTH_TOKEN").expect("Env variable REACTOR_AUTH_TOKEN not set");

	static ref PASSPHRASE: String = env::var("PASSPHRASE").expect("Env variable PASSPHRASE not set");
	static ref PUBLIC_KEY_PEM: String = env::var("PUBLIC_KEY_PEM").expect("Env variable PUBLIC_KEY_PEM not set");
	static ref PRIVATE_KEY_PEM: String = env::var("PRIVATE_KEY_PEM").expect("Env variable PRIVATE_KEY_PEM not set");
}

const TIMEOUT: u64 = 120;

fn new_http_client() -> Client {
	let cb = ClientBuilder::new().timeout(Duration::from_secs(TIMEOUT));
	return cb.build().unwrap();
}

fn encrypt(data: String) -> String {
	let rsa = Rsa::public_key_from_pem(PUBLIC_KEY_PEM.as_bytes()).unwrap();
	let mut buf: Vec<u8> = vec![0; rsa.size() as usize];
	rsa.public_encrypt(data.as_bytes(), &mut buf, Padding::PKCS1).unwrap();
	hex::encode(buf)
}

fn decrypt(hex: String) -> String {
	let rsa = Rsa::private_key_from_pem_passphrase(PRIVATE_KEY_PEM.as_bytes(), PASSPHRASE.as_bytes()).unwrap();
	let mut buf: Vec<u8> = vec![0; rsa.size() as usize];
	let l = rsa.private_decrypt(&hex::decode(hex).unwrap(), &mut buf, Padding::PKCS1).unwrap();
	String::from_utf8(buf[..l].to_vec()).unwrap()
}

#[derive(Deserialize, Serialize)]
struct AuthBody {
	code: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct AuthedUser {
	id: String,
	access_token: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct OAuthAccessBody {
	ok: bool,
	authed_user: Option<AuthedUser>,
	access_token: Option<String>,
	error: Option<String>,
}

async fn auth(Query(auth_body): Query<AuthBody>) -> impl IntoResponse {
	if auth_body.code.eq("") {
		Err((StatusCode::BAD_REQUEST, "No code".to_string()))
	} else {
		match get_access_token(&auth_body.code).await {
			Ok(at) => {
				if at.ok {
					let authed_user = at.authed_user.unwrap();
					match get_authed_user(&authed_user.access_token).await {
						Ok(gu) => {
							let location = format!(
								"{}/api/connected?authorId={}&authorName={}&authorState={}",
								REACTOR_API_PREFIX.as_str(),
								authed_user.id,
								gu,
								encrypt(at.access_token.unwrap())
							);
							Ok((StatusCode::FOUND, [("Location", location)]))
						}
						Err(err_msg) => Err((StatusCode::INTERNAL_SERVER_ERROR, err_msg))
					}
				} else {
					Err((StatusCode::BAD_REQUEST, "Invalid code".to_string()))
				}
			},
			Err(err_msg) => Err((StatusCode::INTERNAL_SERVER_ERROR, err_msg))
		}
	}
}

async fn get_access_token(code: &str) -> Result<OAuthAccessBody, String> {
	let slack_client_id = env::var("SLACK_APP_CLIENT_ID").expect("Env variable SLACK_APP_CLIENT_ID not set");
	let slack_client_secret = env::var("SLACK_APP_CLIENT_SECRET").expect("Env variable SLACK_APP_CLIENT_SECRET not set");

	let params = [
		("client_id", slack_client_id.as_str()),
		("client_secret", slack_client_secret.as_str()),
		("code", &code)
	];

	let response = new_http_client().post("https://slack.com/api/oauth.v2.access")
		.form(&params)
		.send()
		.await;
	match response {
		Ok(r) => {
			let oauth_body = r.json::<OAuthAccessBody>().await;
			match oauth_body {
				Ok(at) => {
					Ok(at)
				}
				Err(_) => {
					Err("Failed to get access token".to_string())
				}
			}
		},
		Err(_) => {
			Err("Failed to get access token".to_string())
		}
	}
}

async fn get_authed_user(access_token: &str) -> Result<String, String> {
	let response = new_http_client().get("https://slack.com/api/users.profile.get")
		.bearer_auth(access_token)
		.send()
		.await;

	match response {
		Ok(res) => {
			match res.text().await {
				Ok(body) => {
					if let Ok(v) = serde_json::from_str::<Value>(&body) {
						Ok(v["profile"]["real_name"].as_str().unwrap().to_string())
					} else {
						Err("Failed to get user's name".to_string())
					}
				}
				Err(_) => {
					Err("Failed to get user's profile".to_string())
				}
			}
		}
		Err(_) => {
			Err("Failed to get user's profile".to_string())
		}
	}
}

#[derive(Deserialize)]
struct EventBody {
	challenge: Option<String>,
	event: Option<Event>,
}

#[derive(Deserialize)]
struct Event {
	bot_id: Option<String>,
	user: Option<String>,
	text: Option<String>,
	files: Option<Vec<File>>,
}

#[derive(Debug, Deserialize)]
struct File {
	name: String,
	mimetype: String,
	url_private: String,
}

async fn capture_event(Json(evt_body): Json<EventBody>) -> impl IntoResponse {
	if let Some(challenge) = evt_body.challenge {
		return (StatusCode::OK, challenge);
	}

	if let Some(evt) = evt_body.event {
		// Only handle message which is sent by user
		if evt.bot_id.is_none() {
			let user = evt.user.unwrap_or_else(|| String::from(""));
			let text = evt.text.unwrap_or_else(|| String::from(""));
			let files = evt.files.unwrap_or_else(|| Vec::new());
			tokio::spawn(post_event_to_reactor(user, text, files));
		}
	}

	(StatusCode::OK, String::new())
}

async fn post_event_to_reactor(user: String, text: String, files: Vec<File>) {

	if files.len() == 0 {
		let request = serde_json::json!({
			"user": user,
			"text": text
		});

		_ = new_http_client().post(format!("{}/api/_funcs/_post", REACTOR_API_PREFIX.as_str()))
			.header("Authorization", REACTOR_AUTH_TOKEN.as_str())
			.json(&request)
			.send()
			.await;
	} else {
		if let Ok(access_token) = get_author_token_from_reactor(&user).await {
			let mut request = multipart::Form::new()
				.text("user", user)
				.text("text", text);

			for f in files.into_iter() {
				if let Ok(b) = get_file(&access_token, &f.url_private).await {
					if let Ok(part) = multipart::Part::bytes(b)
						.file_name(f.name)
						.mime_str(&f.mimetype) {
						request = request.part("file", part);
					}
				}
			}

			_ = new_http_client().post(format!("{}/api/_funcs/_upload", REACTOR_API_PREFIX.as_str()))
				.header("Authorization", REACTOR_AUTH_TOKEN.as_str())
				.multipart(request)
				.send()
				.await;
		}
	}
}

async fn get_author_token_from_reactor(user: &str) -> Result<String, ()> {
	let request = serde_json::json!({
		"author": user
	});

	let response = new_http_client().post(format!("{}/api/_funcs/_author_state", REACTOR_API_PREFIX.as_str()))
		.header("Authorization", REACTOR_AUTH_TOKEN.as_str())
		.json(&request)
		.send()
		.await;

	if let Ok(res) = response {
		if res.status().is_success() {
			if let Ok(body) = res.text().await {
				return Ok(decrypt(body));
			}
		}
	}
	Err(())
}

async fn get_file(access_token: &str, url_private: &str) -> Result<Vec<u8>, ()> {
	let response = new_http_client().get(url_private)
		.bearer_auth(access_token)
		.send()
		.await;
	
	if let Ok(res) = response {
		if res.status().is_success() {
			if let Ok(body) = res.bytes().await {
				return Ok(body.to_vec());
			}
		}
	}

	Err(())
}

#[derive(Deserialize, Serialize)]
struct PostBody {
	user: String,
	text: String,
	state: String,
}

async fn post_msg(Json(msg_body): Json<PostBody>) -> Result<StatusCode, (StatusCode, &'static str)> {
	let request = serde_json::json!({
		"channel": msg_body.user,
		"text": msg_body.text,
	});

	let response = new_http_client().post("https://slack.com/api/chat.postMessage")
		.bearer_auth(decrypt(msg_body.state))
		.json(&request)
		.send()
		.await;
	match response {
		Ok(_) => Ok(StatusCode::OK),
		Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Failed to post message to slack"))
	}
}

async fn upload_file_to_slack(form: multipart::Form, access_token: String) {
	let response = new_http_client().post("https://slack.com/api/files.upload")
		.bearer_auth(decrypt(access_token))
		.multipart(form)
		.send()
		.await;
	if let Ok(res) = response {
		if res.status().is_success() {
			// println!("{:?}", res.text().await);
		}
	}
}


async fn upload_msg(ContentLengthLimit(mut multipart): ContentLengthLimit<Multipart, {10 * 1024 * 1024 /* 250mb */},>) -> impl IntoResponse {
	let mut user = String::new();
	let mut text = String::new();
	let mut state = String::new();

	let mut parts = Vec::new();
	while let Some(field) = multipart.next_field().await.unwrap() {
		let name = field.name().unwrap().to_string();
		match name.as_str() {
			"file" => {
				let file_name = field.file_name().unwrap().to_string();
				let content_type = field.content_type().unwrap().to_string();
				let data = field.bytes().await.unwrap();
				if let Ok(part) = multipart::Part::bytes(data.to_vec())
					.file_name(file_name)
					.mime_str(&content_type) {
					parts.push(part);
				}
			}
			"user" => {
				if let Ok(u) = field.text().await {
					user = u;
				}
			}
			"state" => {
				if let Ok(s) = field.text().await {
					state = s;
				}
			}
			"text" => {
				if let Ok(t) = field.text().await {
					text = t;
				}
			}
			_ => {}
		}
	}

	if user.len() == 0 || state.len() == 0 {
		return Err((StatusCode::BAD_REQUEST, ""));
	}

	if parts.len() > 0 {
		for part in parts.into_iter() {
			let mut form = multipart::Form::new()
				.text("channels", user.clone());
			form = form.part("file", part);
			upload_file_to_slack(form, state.clone()).await;
		}
	}

	if text.len() > 0 {
		return post_msg(Json::from(PostBody {
			user: user,
			state: state,
			text: text,
		})).await;
	} else {
		return Ok(StatusCode::OK);
	}
}

#[tokio::main]
async fn main() {
	let app = Router::new()
		.route("/auth", get(auth))
		.route("/event", post(capture_event))
		.route("/post", post(post_msg).put(upload_msg));

	let port = env::var("PORT").unwrap_or_else(|_| "8090".to_string());
	let port = port.parse::<u16>().unwrap();
	let addr = SocketAddr::from(([127, 0, 0, 1], port));

	axum::Server::bind(&addr)
		.serve(app.into_make_service())
		.await
		.unwrap();
}