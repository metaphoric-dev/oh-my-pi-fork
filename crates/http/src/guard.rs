//! Opt-in address guard for clients that fetch agent-authored URLs.
//!
//! The guard always refuses link-local, unspecified, multicast, and broadcast
//! addresses, as URL literals and as resolved addresses. A redirect may not
//! move from a globally routable address to a non-public one; requests whose
//! authored host is loopback or private fetch normally, and redirects that
//! stay inside the non-public class stay allowed.

use std::{
	cell::Cell,
	error::Error as StdError,
	future::Future,
	net::{IpAddr, SocketAddr, ToSocketAddrs},
};

use omp_core::Str;
use reqwest::{
	ClientBuilder,
	dns::{Addrs, Name, Resolve, Resolving},
	redirect::{Attempt, Policy},
};
use thiserror::Error;
use tokio::task::spawn_blocking;
use url::{Host, Url};

use crate::client_builder;

/// Safety class of one address a guarded client could connect to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressClass {
	/// Link-local, unspecified, multicast, or broadcast: never connected.
	Refused,
	/// Loopback, private, or otherwise not globally routable.
	NonPublic,
	/// Globally routable.
	Global,
}

/// A refused address or forbidden redirect target, naming its host only.
#[derive(Debug, Error)]
#[error("{host}")]
pub struct AddressGuardError {
	/// Host of the refused target: an IP literal or a DNS name.
	pub host: Str,
}

tokio::task_local! {
	static GUARD_CLASS: Cell<Option<AddressClass>>;
}

/// Classifies `ip` for the address guard.
pub fn address_class(ip: IpAddr) -> AddressClass {
	if refused_address(ip) {
		return AddressClass::Refused;
	}
	if globally_routable(ip) {
		return AddressClass::Global;
	}
	AddressClass::NonPublic
}

fn refused_address(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(value) => {
			value.is_link_local()
				|| value.is_unspecified()
				|| value.is_multicast()
				|| value.is_broadcast()
		},
		IpAddr::V6(value) => {
			if let Some(mapped) = value.to_ipv4_mapped() {
				return refused_address(IpAddr::V4(mapped));
			}
			value.is_unicast_link_local() || value.is_unspecified() || value.is_multicast()
		},
	}
}

/// Whether `ip` is outside loopback, private, link-local, and reserved ranges.
pub fn globally_routable(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(value) => {
			!(value.is_private()
				|| value.is_loopback()
				|| value.is_link_local()
				|| value.is_multicast()
				|| value.is_unspecified()
				|| value.is_broadcast()
				|| value.octets()[0] == 0
				|| matches!(
					value.octets(),
					[100, 64..=127, _, _]
						| [192, 0, 0 | 2, _]
						| [198, 18..=19, _, _]
						| [198, 51, 100, _]
						| [203, 0, 113, _]
						| [240..=255, _, _, _]
						| [168, 63, 129, 16]
				))
		},
		IpAddr::V6(value) => {
			if let Some(mapped) = value.to_ipv4_mapped() {
				return globally_routable(IpAddr::V4(mapped));
			}
			!(value.is_loopback()
				|| value.is_multicast()
				|| value.is_unspecified()
				|| value.is_unique_local()
				|| value.is_unicast_link_local()
				|| matches!(value.segments(), [0x2001, 0xdb8, ..]))
		},
	}
}

/// Returns the IP address when `url`'s host is a literal, not a name.
pub fn literal_address(url: &Url) -> Option<IpAddr> {
	match url.host() {
		Some(Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
		Some(Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
		_ => None,
	}
}

/// Starts a client builder whose resolution and redirect hops are guarded.
///
/// The `max_redirects` cap replaces `redirect::Policy::limited`; a custom
/// policy has to enforce the cap itself. Idle connections are not retained:
/// a reused connection would skip resolution and the guard with it, so every
/// hop opens a fresh, resolved connection. Environment proxies are ignored:
/// a proxy resolves the target itself, so honoring one would skip the checks.
pub fn guarded_client_builder(max_redirects: usize) -> ClientBuilder {
	client_builder()
		.no_proxy()
		.dns_resolver(AddressGuardResolver)
		.redirect(address_guard_policy(max_redirects))
		.pool_max_idle_per_host(0)
}

fn address_guard_policy(max_redirects: usize) -> Policy {
	Policy::custom(move |attempt: Attempt| {
		if attempt.previous().len() > max_redirects {
			return attempt.error("too many redirects");
		}
		let host = attempt.url().host_str().unwrap_or_default().to_owned();
		let Some(ip) = literal_address(attempt.url()) else {
			if !host.is_empty() {
				record_global_when_unset();
			}
			return attempt.follow();
		};
		let class = address_class(ip);
		if redirect_refusal(recorded_class(), class) {
			return attempt.error(AddressGuardError { host: Str::new(&host) });
		}
		record_class(class);
		attempt.follow()
	})
}

/// Whether a redirect hop to a `target`-class address is forbidden.
///
/// Refused classes are never allowed. A hop into the non-public class is
/// allowed only when the previous hop's address was non-public too; an
/// unrecorded origin is treated as globally routable, because the policy only
/// runs on redirect hops.
fn redirect_refusal(previous: Option<AddressClass>, target: AddressClass) -> bool {
	target == AddressClass::Refused
		|| target == AddressClass::NonPublic && !matches!(previous, Some(AddressClass::NonPublic))
}

/// Whether connecting to a `resolved`-class address is forbidden for a hop
/// whose predecessor used `previous`.
///
/// Resolution without a recorded predecessor is the request's first hop,
/// where non-public targets stay allowed.
fn resolution_refusal(previous: Option<AddressClass>, resolved: AddressClass) -> bool {
	resolved == AddressClass::Refused
		|| resolved == AddressClass::NonPublic && matches!(previous, Some(AddressClass::Global))
}

/// Runs `fut` inside one guarded request scope.
///
/// `origin` is the class of the authored URL's IP literal host, or `None`
/// when the host is a name and its class becomes known at resolution.
pub fn address_guard_scope<T: 'static>(
	origin: Option<AddressClass>,
	fut: impl Future<Output = T>,
) -> impl Future<Output = T> {
	GUARD_CLASS.scope(Cell::new(origin), fut)
}

fn recorded_class() -> Option<AddressClass> {
	GUARD_CLASS.try_with(|cell| cell.get()).unwrap_or(None)
}

fn record_class(class: AddressClass) {
	let _ = GUARD_CLASS.try_with(|cell| cell.set(Some(class)));
}

fn record_global_when_unset() {
	let _ = GUARD_CLASS.try_with(|cell| {
		if cell.get().is_none() {
			cell.set(Some(AddressClass::Global));
		}
	});
}

struct AddressGuardResolver;

impl Resolve for AddressGuardResolver {
	fn resolve(&self, name: Name) -> Resolving {
		let host = Str::new(name.as_str());
		Box::pin(async move {
			let resolved = system_addresses(host.clone()).await?;
			let Some(class) = decide_resolved(recorded_class(), &resolved) else {
				return Err(guard_error(host));
			};
			record_class(class);
			Ok(Box::new(resolved.into_iter()) as Addrs)
		})
	}
}

/// Pure decision behind the guarded resolver, tested with injected addresses.
///
/// Returns the recorded class when every resolved address shares one class
/// that the hop from `previous` may use, or `None` when the connector must
/// not use the set: empty, mixed-class, refused-class, or a downgrade from
/// a globally routable previous hop to a non-public address.
fn decide_resolved(previous: Option<AddressClass>, addrs: &[SocketAddr]) -> Option<AddressClass> {
	let class = resolved_class(addrs)?;
	if resolution_refusal(previous, class) {
		return None;
	}
	Some(class)
}

async fn system_addresses(host: Str) -> Result<Vec<SocketAddr>, Box<dyn StdError + Send + Sync>> {
	let joined = spawn_blocking(move || {
		(host.as_str(), 0)
			.to_socket_addrs()
			.map(|addrs| addrs.collect::<Vec<_>>())
	})
	.await;
	match joined {
		Ok(Ok(resolved)) => Ok(resolved),
		Ok(Err(error)) => Err(Box::new(error)),
		Err(error) => Err(Box::new(error)),
	}
}

/// The one class shared by every resolved address, or `None` when the set is
/// empty or mixes classes: both are refused instead of letting the connector
/// pick an address whose class differs from the recorded one.
fn resolved_class(addrs: &[SocketAddr]) -> Option<AddressClass> {
	let mut class = None;
	for addr in addrs {
		let current = address_class(addr.ip());
		match class {
			None => class = Some(current),
			Some(previous) if previous != current => return None,
			Some(_) => {},
		}
	}
	class
}

fn guard_error(host: Str) -> Box<dyn StdError + Send + Sync> {
	Box::new(AddressGuardError { host })
}

#[cfg(test)]
mod tests {
	use super::*;

	fn class(ip: &str) -> AddressClass {
		address_class(ip.parse().expect("test address"))
	}

	#[test]
	fn classifies_addresses() {
		let refused = [
			"169.254.169.254",
			"169.254.1.1",
			"fe80::1",
			"febf::1",
			"0.0.0.0",
			"::",
			"224.0.0.1",
			"ff02::1",
			"255.255.255.255",
			"::ffff:169.254.169.254",
		];
		for ip in refused {
			assert_eq!(class(ip), AddressClass::Refused, "{ip}");
		}
		let non_public = [
			"127.0.0.1",
			"::1",
			"10.0.0.1",
			"10.0.0.5",
			"172.16.0.1",
			"192.168.1.1",
			"fd00::1",
			"100.64.0.1",
			"192.0.2.1",
			"198.18.0.1",
			"2001:db8::1",
			"::ffff:127.0.0.1",
		];
		for ip in non_public {
			assert_eq!(class(ip), AddressClass::NonPublic, "{ip}");
		}
		for ip in ["93.184.216.34", "8.8.8.8", "1.1.1.1", "2606:2800:220:1::", "2606:4700:4700::1111", "::ffff:8.8.8.8"] {
			assert_eq!(class(ip), AddressClass::Global, "{ip}");
		}
	}

	#[test]
	fn redirect_policy_refuses_downgrades_and_refused_classes() {
		assert!(!redirect_refusal(None, AddressClass::Global));
		assert!(redirect_refusal(None, AddressClass::NonPublic));
		assert!(redirect_refusal(None, AddressClass::Refused));
		assert!(!redirect_refusal(Some(AddressClass::Global), AddressClass::Global));
		assert!(redirect_refusal(Some(AddressClass::Global), AddressClass::NonPublic));
		assert!(redirect_refusal(Some(AddressClass::Global), AddressClass::Refused));
		assert!(!redirect_refusal(Some(AddressClass::NonPublic), AddressClass::NonPublic));
		assert!(!redirect_refusal(Some(AddressClass::NonPublic), AddressClass::Global));
		assert!(redirect_refusal(Some(AddressClass::NonPublic), AddressClass::Refused));
	}

	#[test]
	fn resolution_refusal_allows_unrecorded_first_hops() {
		assert!(!resolution_refusal(None, AddressClass::NonPublic));
		assert!(!resolution_refusal(None, AddressClass::Global));
		assert!(resolution_refusal(None, AddressClass::Refused));
		assert!(resolution_refusal(Some(AddressClass::Global), AddressClass::NonPublic));
		assert!(!resolution_refusal(Some(AddressClass::NonPublic), AddressClass::NonPublic));
	}

	#[test]
	fn resolver_decision_uses_injected_addresses() {
		fn addrs<const N: usize>(hosts: [&str; N]) -> Vec<SocketAddr> {
			hosts
				.into_iter()
				.map(|host| {
					let text =
						if host.contains(':') { format!("[{host}]:80") } else { format!("{host}:80") };
					text.parse().expect("test address")
				})
				.collect()
		}
		let global = addrs(["93.184.216.34", "2606:2800:220:1::"]);
		let loopback = addrs(["127.0.0.1"]);
		let mapped_loopback = addrs(["::ffff:127.0.0.1"]);
		let link_local = addrs(["169.254.169.254"]);
		assert_eq!(decide_resolved(None, &global), Some(AddressClass::Global));
		assert_eq!(decide_resolved(None, &loopback), Some(AddressClass::NonPublic));
		assert_eq!(decide_resolved(Some(AddressClass::Global), &loopback), None);
		assert_eq!(decide_resolved(Some(AddressClass::Global), &mapped_loopback), None);
		assert_eq!(decide_resolved(Some(AddressClass::Global), &global), Some(AddressClass::Global));
		assert_eq!(decide_resolved(Some(AddressClass::NonPublic), &loopback), Some(AddressClass::NonPublic));
		assert_eq!(decide_resolved(Some(AddressClass::NonPublic), &link_local), None);
		assert_eq!(decide_resolved(None, &link_local), None);
		assert_eq!(decide_resolved(None, &[]), None);
	}

	#[test]
	fn localhost_resolves_to_non_public_addresses() {
		use std::net::ToSocketAddrs as _;
		let resolved: Vec<SocketAddr> =
			("localhost", 0).to_socket_addrs().expect("resolve localhost").collect();
		assert!(!resolved.is_empty(), "localhost must resolve to at least one address");
		for addr in &resolved {
			assert_eq!(address_class(addr.ip()), AddressClass::NonPublic, "{addr}");
		}
		assert_eq!(decide_resolved(Some(AddressClass::Global), &resolved), None);
	}

	#[test]
	fn resolved_class_refuses_empty_and_mixed_sets() {
		let one = "1.1.1.1:80".parse().expect("test address");
		let other = "8.8.8.8:80".parse().expect("test address");
		let private = "10.0.0.5:80".parse().expect("test address");
		assert_eq!(resolved_class(&[]), None);
		assert_eq!(resolved_class(&[one, other]), Some(AddressClass::Global));
		assert_eq!(resolved_class(&[one, private]), None);
	}
}
