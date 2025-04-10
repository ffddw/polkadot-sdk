// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::{collections::HashSet, str::FromStr, sync::Arc, thread::sleep};

use crate::NetworkProvider;
use codec::{Decode, Encode};
use futures::Future;
pub use http::SharedClient;
use sc_network::Multiaddr;
use sc_network_types::PeerId;
use sp_core::{
	offchain::{
		self, HttpError, HttpRequestId, HttpRequestStatus, OpaqueMultiaddr, OpaqueNetworkState,
		Timestamp,
	},
	OpaquePeerId,
};

mod http;

mod timestamp;

/// Asynchronous offchain API.
///
/// NOTE this is done to prevent recursive calls into the runtime
/// (which are not supported currently).
pub(crate) struct Api {
	/// A provider for substrate networking.
	network_provider: Arc<dyn NetworkProvider + Send + Sync>,
	/// Is this node a potential validator?
	is_validator: bool,
	/// Everything HTTP-related is handled by a different struct.
	http: http::HttpApi,
}

impl offchain::Externalities for Api {
	fn is_validator(&self) -> bool {
		self.is_validator
	}

	fn network_state(&self) -> Result<OpaqueNetworkState, ()> {
		let external_addresses = self.network_provider.external_addresses();

		let state = NetworkState::new(self.network_provider.local_peer_id(), external_addresses);
		Ok(OpaqueNetworkState::from(state))
	}

	fn timestamp(&mut self) -> Timestamp {
		timestamp::now()
	}

	fn sleep_until(&mut self, deadline: Timestamp) {
		sleep(timestamp::timestamp_from_now(deadline));
	}

	fn random_seed(&mut self) -> [u8; 32] {
		rand::random()
	}

	fn http_request_start(
		&mut self,
		method: &str,
		uri: &str,
		_meta: &[u8],
	) -> Result<HttpRequestId, ()> {
		self.http.request_start(method, uri)
	}

	fn http_request_add_header(
		&mut self,
		request_id: HttpRequestId,
		name: &str,
		value: &str,
	) -> Result<(), ()> {
		self.http.request_add_header(request_id, name, value)
	}

	fn http_request_write_body(
		&mut self,
		request_id: HttpRequestId,
		chunk: &[u8],
		deadline: Option<Timestamp>,
	) -> Result<(), HttpError> {
		self.http.request_write_body(request_id, chunk, deadline)
	}

	fn http_response_wait(
		&mut self,
		ids: &[HttpRequestId],
		deadline: Option<Timestamp>,
	) -> Vec<HttpRequestStatus> {
		self.http.response_wait(ids, deadline)
	}

	fn http_response_headers(&mut self, request_id: HttpRequestId) -> Vec<(Vec<u8>, Vec<u8>)> {
		self.http.response_headers(request_id)
	}

	fn http_response_read_body(
		&mut self,
		request_id: HttpRequestId,
		buffer: &mut [u8],
		deadline: Option<Timestamp>,
	) -> Result<usize, HttpError> {
		self.http.response_read_body(request_id, buffer, deadline)
	}

	fn set_authorized_nodes(&mut self, nodes: Vec<OpaquePeerId>, authorized_only: bool) {
		let peer_ids: HashSet<PeerId> =
			nodes.into_iter().filter_map(|node| PeerId::from_bytes(&node.0).ok()).collect();

		self.network_provider.set_authorized_peers(peer_ids);
		self.network_provider.set_authorized_only(authorized_only);
	}
}

/// Information about the local node's network state.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct NetworkState {
	peer_id: PeerId,
	external_addresses: Vec<Multiaddr>,
}

impl NetworkState {
	fn new(peer_id: PeerId, external_addresses: Vec<Multiaddr>) -> Self {
		NetworkState { peer_id, external_addresses }
	}
}

impl From<NetworkState> for OpaqueNetworkState {
	fn from(state: NetworkState) -> OpaqueNetworkState {
		let enc = Encode::encode(&state.peer_id.to_bytes());
		let peer_id = OpaquePeerId::new(enc);

		let external_addresses: Vec<OpaqueMultiaddr> = state
			.external_addresses
			.iter()
			.map(|multiaddr| {
				let e = Encode::encode(&multiaddr.to_string());
				OpaqueMultiaddr::new(e)
			})
			.collect();

		OpaqueNetworkState { peer_id, external_addresses }
	}
}

impl TryFrom<OpaqueNetworkState> for NetworkState {
	type Error = ();

	fn try_from(state: OpaqueNetworkState) -> Result<Self, Self::Error> {
		let inner_vec = state.peer_id.0;

		let bytes: Vec<u8> = Decode::decode(&mut &inner_vec[..]).map_err(|_| ())?;
		let peer_id = PeerId::from_bytes(&bytes).map_err(|_| ())?;

		let external_addresses: Result<Vec<Multiaddr>, Self::Error> = state
			.external_addresses
			.iter()
			.map(|enc_multiaddr| -> Result<Multiaddr, Self::Error> {
				let inner_vec = &enc_multiaddr.0;
				let bytes = <Vec<u8>>::decode(&mut &inner_vec[..]).map_err(|_| ())?;
				let multiaddr_str = String::from_utf8(bytes).map_err(|_| ())?;
				let multiaddr = Multiaddr::from_str(&multiaddr_str).map_err(|_| ())?;
				Ok(multiaddr)
			})
			.collect();
		let external_addresses = external_addresses?;

		Ok(NetworkState { peer_id, external_addresses })
	}
}

/// Offchain extensions implementation API
///
/// This is the asynchronous processing part of the API.
pub(crate) struct AsyncApi {
	/// Everything HTTP-related is handled by a different struct.
	http: Option<http::HttpWorker>,
}

impl AsyncApi {
	/// Creates new Offchain extensions API implementation and the asynchronous processing part.
	pub fn new(
		network_provider: Arc<dyn NetworkProvider + Send + Sync>,
		is_validator: bool,
		shared_http_client: SharedClient,
	) -> (Api, Self) {
		let (http_api, http_worker) = http::http(shared_http_client);

		let api = Api { network_provider, is_validator, http: http_api };

		let async_api = Self { http: Some(http_worker) };

		(api, async_api)
	}

	/// Run a processing task for the API
	pub fn process(self) -> impl Future<Output = ()> {
		self.http.expect("`process` is only called once; qed")
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sc_client_db::offchain::LocalStorage;
	use sc_network::{
		config::MultiaddrWithPeerId, types::ProtocolName, NetworkPeers, NetworkStateInfo,
		ObservedRole, ReputationChange,
	};
	use sp_core::offchain::{storage::OffchainDb, DbExternalities, Externalities, StorageKind};
	use std::time::SystemTime;

	pub(super) struct TestNetwork();

	#[async_trait::async_trait]
	impl NetworkPeers for TestNetwork {
		fn set_authorized_peers(&self, _peers: HashSet<PeerId>) {
			unimplemented!();
		}

		fn set_authorized_only(&self, _reserved_only: bool) {
			unimplemented!();
		}

		fn add_known_address(&self, _peer_id: PeerId, _addr: Multiaddr) {
			unimplemented!();
		}

		fn report_peer(&self, _peer_id: PeerId, _cost_benefit: ReputationChange) {
			unimplemented!();
		}

		fn peer_reputation(&self, _peer_id: &PeerId) -> i32 {
			unimplemented!()
		}

		fn disconnect_peer(&self, _peer_id: PeerId, _protocol: ProtocolName) {
			unimplemented!();
		}

		fn accept_unreserved_peers(&self) {
			unimplemented!();
		}

		fn deny_unreserved_peers(&self) {
			unimplemented!();
		}

		fn add_reserved_peer(&self, _peer: MultiaddrWithPeerId) -> Result<(), String> {
			unimplemented!();
		}

		fn remove_reserved_peer(&self, _peer_id: PeerId) {
			unimplemented!();
		}

		fn set_reserved_peers(
			&self,
			_protocol: ProtocolName,
			_peers: HashSet<Multiaddr>,
		) -> Result<(), String> {
			unimplemented!();
		}

		fn add_peers_to_reserved_set(
			&self,
			_protocol: ProtocolName,
			_peers: HashSet<Multiaddr>,
		) -> Result<(), String> {
			unimplemented!();
		}

		fn remove_peers_from_reserved_set(
			&self,
			_protocol: ProtocolName,
			_peers: Vec<PeerId>,
		) -> Result<(), String> {
			unimplemented!();
		}

		fn sync_num_connected(&self) -> usize {
			unimplemented!();
		}

		fn peer_role(&self, _peer_id: PeerId, _handshake: Vec<u8>) -> Option<ObservedRole> {
			None
		}

		async fn reserved_peers(&self) -> Result<Vec<PeerId>, ()> {
			unimplemented!();
		}
	}

	impl NetworkStateInfo for TestNetwork {
		fn external_addresses(&self) -> Vec<Multiaddr> {
			Vec::new()
		}

		fn local_peer_id(&self) -> PeerId {
			PeerId::random()
		}

		fn listen_addresses(&self) -> Vec<Multiaddr> {
			Vec::new()
		}
	}

	fn offchain_api() -> (Api, AsyncApi) {
		sp_tracing::try_init_simple();
		let mock = Arc::new(TestNetwork());
		let shared_client = SharedClient::new().unwrap();

		AsyncApi::new(mock, false, shared_client)
	}

	fn offchain_db() -> OffchainDb<LocalStorage> {
		OffchainDb::new(LocalStorage::new_test())
	}

	#[test]
	fn should_get_timestamp() {
		let mut api = offchain_api().0;

		// Get timestamp from std.
		let now = SystemTime::now();
		let d: u64 = now
			.duration_since(SystemTime::UNIX_EPOCH)
			.unwrap()
			.as_millis()
			.try_into()
			.unwrap();

		// Get timestamp from offchain api.
		let timestamp = api.timestamp();

		// Compare.
		assert!(timestamp.unix_millis() > 0);
		assert!(timestamp.unix_millis() >= d);
	}

	#[test]
	fn should_sleep() {
		let mut api = offchain_api().0;

		// Arrange.
		let now = api.timestamp();
		let delta = sp_core::offchain::Duration::from_millis(100);
		let deadline = now.add(delta);

		// Act.
		api.sleep_until(deadline);
		let new_now = api.timestamp();

		// Assert.
		// The diff could be more than the sleep duration.
		assert!(new_now.unix_millis() - 100 >= now.unix_millis());
	}

	#[test]
	fn should_set_get_and_clear_local_storage() {
		// given
		let kind = StorageKind::PERSISTENT;
		let mut api = offchain_db();
		let key = b"test";

		// when
		assert_eq!(api.local_storage_get(kind, key), None);
		api.local_storage_set(kind, key, b"value");

		// then
		assert_eq!(api.local_storage_get(kind, key), Some(b"value".to_vec()));

		// when
		api.local_storage_clear(kind, key);

		// then
		assert_eq!(api.local_storage_get(kind, key), None);
	}

	#[test]
	fn should_compare_and_set_local_storage() {
		// given
		let kind = StorageKind::PERSISTENT;
		let mut api = offchain_db();
		let key = b"test";
		api.local_storage_set(kind, key, b"value");

		// when
		assert_eq!(api.local_storage_compare_and_set(kind, key, Some(b"val"), b"xxx"), false);
		assert_eq!(api.local_storage_get(kind, key), Some(b"value".to_vec()));

		// when
		assert_eq!(api.local_storage_compare_and_set(kind, key, Some(b"value"), b"xxx"), true);
		assert_eq!(api.local_storage_get(kind, key), Some(b"xxx".to_vec()));
	}

	#[test]
	fn should_compare_and_set_local_storage_with_none() {
		// given
		let kind = StorageKind::PERSISTENT;
		let mut api = offchain_db();
		let key = b"test";

		// when
		let res = api.local_storage_compare_and_set(kind, key, None, b"value");

		// then
		assert_eq!(res, true);
		assert_eq!(api.local_storage_get(kind, key), Some(b"value".to_vec()));
	}

	#[test]
	fn should_convert_network_states() {
		// given
		let state = NetworkState::new(
			PeerId::random(),
			vec![
				Multiaddr::try_from("/ip4/127.0.0.1/tcp/1234".to_string()).unwrap(),
				Multiaddr::try_from("/ip6/2601:9:4f81:9700:803e:ca65:66e8:c21").unwrap(),
			],
		);

		// when
		let opaque_state = OpaqueNetworkState::from(state.clone());
		let converted_back_state = NetworkState::try_from(opaque_state).unwrap();

		// then
		assert_eq!(state, converted_back_state);
	}

	#[test]
	fn should_get_random_seed() {
		// given
		let mut api = offchain_api().0;
		let seed = api.random_seed();
		// then
		assert_ne!(seed, [0; 32]);
	}
}
