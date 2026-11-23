//! Public contract tests for the address guard.
//!
//! These tests exercise only the exported client API over real TCP servers:
//! they are the guard's public contract, so they live here rather than beside
//! the private decision functions in the unit tests.

use std::net::{Ipv4Addr, SocketAddr};

use omp_http::{AddressClass, AddressGuardError, address_guard_scope, guarded_client_builder};
use tokio::{
	io::{AsyncReadExt as _, AsyncWriteExt as _},
	net::TcpListener,
};

async fn serve_response(response: String) -> SocketAddr {
	let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
		.await
		.expect("bind guard test server");
	let address = listener.local_addr().expect("guard test server address");
	tokio::spawn(async move {
		while let Ok((mut stream, _)) = listener.accept().await {
			let response = response.clone();
			tokio::spawn(async move {
				let mut buffer = [0u8; 2048];
				let _ = stream.read(&mut buffer).await;
				let _ = stream.write_all(response.as_bytes()).await;
			});
		}
	});
	address
}

fn redirect_response(location: &str) -> String {
	format!(
		"HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: \
		 close\r\n\r\n"
	)
}

const OK_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: \
                           2\r\nConnection: close\r\n\r\nok";

async fn fetch(url: &str, origin: Option<AddressClass>) -> Result<String, reqwest::Error> {
	let client = guarded_client_builder(20)
		.build()
		.expect("build guarded client");
	address_guard_scope(origin, async {
		let response = client.get(url).send().await?;
		Ok::<_, reqwest::Error>(response.text().await.expect("read guard test body"))
	})
	.await
}

fn guard_error_of(error: &reqwest::Error) -> Option<String> {
	let mut source = std::error::Error::source(error);
	while let Some(current) = source {
		if let Some(blocked) = current.downcast_ref::<AddressGuardError>() {
			return Some(blocked.host.to_string());
		}
		source = current.source();
	}
	None
}

#[tokio::test]
async fn refuses_redirect_to_link_local_literal() {
	let origin = serve_response(redirect_response("http://169.254.169.254/latest/meta-data/")).await;
	let error = fetch(&format!("http://{origin}/start"), Some(AddressClass::NonPublic))
		.await
		.expect_err("link-local redirect must be refused");
	assert_eq!(guard_error_of(&error), Some("169.254.169.254".to_owned()));
}

#[tokio::test]
async fn follows_redirect_inside_loopback() {
	let target = serve_response(OK_RESPONSE.to_owned()).await;
	let origin = serve_response(redirect_response(&format!("http://{target}/final"))).await;
	let body = fetch(&format!("http://{origin}/start"), Some(AddressClass::NonPublic))
		.await
		.expect("loopback redirect must follow");
	assert_eq!(body, "ok");
}

#[tokio::test]
async fn follows_loopback_redirect_from_hostname_origin() {
	let target = serve_response(OK_RESPONSE.to_owned()).await;
	let origin = serve_response(redirect_response(&format!("http://{target}/final"))).await;
	let body = fetch(&format!("http://localhost:{}/start", origin.port()), None)
		.await
		.expect("hostname origin loopback redirect must follow");
	assert_eq!(body, "ok");
}

#[tokio::test]
async fn refuses_resolution_downgrade_from_global_origin() {
	let origin = serve_response(OK_RESPONSE.to_owned()).await;
	let error =
		fetch(&format!("http://localhost:{}/start", origin.port()), Some(AddressClass::Global))
			.await
			.expect_err("global-to-non-public downgrade must be refused");
	assert_eq!(guard_error_of(&error), Some("localhost".to_owned()));
}

#[tokio::test]
async fn guards_refused_redirect_without_an_explicit_scope() {
	let origin = serve_response(redirect_response("http://169.254.169.254/latest/meta-data/")).await;
	let client = guarded_client_builder(20)
		.build()
		.expect("build guarded client");
	let error = client
		.get(format!("http://{origin}/start"))
		.send()
		.await
		.expect_err("refused-class redirect must be refused even without a scope");
	assert_eq!(guard_error_of(&error), Some("169.254.169.254".to_owned()));
}

#[tokio::test]
async fn caps_redirect_chain_at_the_limit() {
	let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
		.await
		.expect("bind guard loop server");
	let address = listener.local_addr().expect("guard loop server address");
	let response = redirect_response(&format!("http://{address}/next"));
	tokio::spawn(async move {
		while let Ok((mut stream, _)) = listener.accept().await {
			let response = response.clone();
			tokio::spawn(async move {
				let mut buffer = [0u8; 2048];
				let _ = stream.read(&mut buffer).await;
				let _ = stream.write_all(response.as_bytes()).await;
			});
		}
	});
	let error = fetch(&format!("http://{address}/start"), Some(AddressClass::NonPublic))
		.await
		.expect_err("redirect loop must hit the cap");
	assert!(guard_error_of(&error).is_none());
	assert!(error.is_redirect());
}
