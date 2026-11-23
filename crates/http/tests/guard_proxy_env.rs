//! The guarded client ignores environment proxies.
//!
//! A forward proxy resolves the target itself, so a guarded read that honored
//! one would skip the address checks: the proxy host is what gets resolved
//! locally, never the redirect target. This file runs as its own test binary
//! so the process environment changes below cannot leak into other tests.

use std::net::Ipv4Addr;

use omp_http::{AddressClass, address_guard_scope, guarded_client_builder};
use tokio::{
	io::{AsyncReadExt as _, AsyncWriteExt as _},
	net::TcpListener,
};

const DIRECT_BODY: &str = "ok";
const PROXIED_BODY: &str = "proxied";

async fn serve(response: String) -> std::net::SocketAddr {
	let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
		.await
		.expect("bind test server");
	let address = listener.local_addr().expect("test server address");
	tokio::spawn(async move {
		while let Ok((mut stream, _)) = listener.accept().await {
			let response = response.clone();
			tokio::spawn(async move {
				let mut buffer = [0u8; 4096];
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

fn ok_response(body: &str) -> String {
	format!(
		"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: \
		 close\r\n\r\n{}",
		body.len(),
		body
	)
}

/// A proxy that answers every request with canned content without forwarding.
/// A guarded fetch that reaches it proves the read used the proxy.
async fn fake_proxy() -> (std::net::SocketAddr, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
	use std::sync::{Arc, atomic::AtomicUsize};
	let hits = Arc::new(AtomicUsize::new(0));
	let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
		.await
		.expect("bind fake proxy");
	let address = listener.local_addr().expect("fake proxy address");
	let seen = Arc::clone(&hits);
	tokio::spawn(async move {
		while let Ok((mut stream, _)) = listener.accept().await {
			seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
			tokio::spawn(async move {
				let mut buffer = [0u8; 4096];
				let _ = stream.read(&mut buffer).await;
				let _ = stream.write_all(ok_response(PROXIED_BODY).as_bytes()).await;
			});
		}
	});
	(address, hits)
}

struct ProxyEnv {
	previous: Vec<(String, Option<String>)>,
}

impl ProxyEnv {
	fn set_through(proxy: &std::net::SocketAddr) -> Self {
		let names = [
			"HTTP_PROXY",
			"http_proxy",
			"HTTPS_PROXY",
			"https_proxy",
			"ALL_PROXY",
			"all_proxy",
			"NO_PROXY",
			"no_proxy",
			"REQUEST_METHOD",
		];
		let previous = names
			.iter()
			.map(|name| ((*name).to_owned(), std::env::var(name).ok()))
			.collect();
		for name in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
			unsafe { std::env::set_var(name, format!("http://{proxy}")) };
		}
		for name in ["ALL_PROXY", "all_proxy", "NO_PROXY", "no_proxy", "REQUEST_METHOD"] {
			unsafe { std::env::remove_var(name) };
		}
		Self { previous }
	}
}

impl Drop for ProxyEnv {
	fn drop(&mut self) {
		for (name, value) in std::mem::take(&mut self.previous) {
			match value {
				Some(text) => unsafe { std::env::set_var(&name, text) },
				None => unsafe { std::env::remove_var(&name) },
			}
		}
	}
}

#[tokio::test]
async fn guarded_reads_ignore_environment_proxies() {
	let target = serve(ok_response(DIRECT_BODY)).await;
	let origin = serve(redirect_response(&format!("http://{target}/final"))).await;
	let (proxy, hits) = fake_proxy().await;
	let _env = ProxyEnv::set_through(&proxy);

	// Control: an unguarded client honors the proxy variables, so the proxy
	// below is live and would have intercepted the guarded read.
	let plain = omp_http::client_builder()
		.build()
		.expect("build plain client");
	let control = plain
		.get(format!("http://{origin}/start"))
		.send()
		.await
		.expect("control fetch through the proxy");
	assert_eq!(
		control.text().await.expect("read control body"),
		PROXIED_BODY,
		"the proxy variables must intercept an unguarded read or this test is vacuous"
	);
	assert!(hits.load(std::sync::atomic::Ordering::SeqCst) > 0, "the fake proxy must see traffic");

	// The guarded read must go direct: it follows the redirect itself and
	// never answers with the proxy's canned body. The scope is seeded from
	// the literal origin exactly as the URL reader does.
	let guarded = guarded_client_builder(20)
		.build()
		.expect("build guarded client");
	let body = address_guard_scope(Some(AddressClass::NonPublic), async {
		guarded
			.get(format!("http://{origin}/start"))
			.send()
			.await
			.expect("guarded read must succeed direct")
			.text()
			.await
			.expect("read guarded body")
	})
	.await;
	assert_eq!(body, DIRECT_BODY);
	assert!(!body.contains(PROXIED_BODY), "guarded read answered with proxy content");
}
