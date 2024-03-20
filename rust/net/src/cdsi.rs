//
// Copyright 2023 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::default::Default;
use std::fmt::Display;
use std::num::{NonZeroU64, ParseIntError};
use std::str::FromStr;

use libsignal_core::{Aci, Pni};
use prost::Message as _;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_boring::SslStream;
use uuid::Uuid;

use crate::auth::HttpBasicAuth;
use crate::enclave::{Cdsi, EnclaveEndpointConnection};
use crate::infra::connection_manager::ConnectionManager;
use crate::infra::errors::TransportConnectError;
use crate::infra::ws::{
    AttestedConnection, AttestedConnectionError, NextOrClose, WebSocketConnectError,
    WebSocketServiceError,
};
use crate::infra::{AsyncDuplexStream, TransportConnector};
use crate::proto::cds2::{ClientRequest, ClientResponse};

trait FixedLengthSerializable {
    const SERIALIZED_LEN: usize;

    // TODO: when feature(generic_const_exprs) is stabilized, make the target an
    // array reference instead of a slice.
    fn serialize_into(&self, target: &mut [u8]);
}

trait CollectSerialized {
    fn collect_serialized(self) -> Vec<u8>;
}

impl<It: ExactSizeIterator<Item = T>, T: FixedLengthSerializable> CollectSerialized for It {
    fn collect_serialized(self) -> Vec<u8> {
        let mut output = vec![0; T::SERIALIZED_LEN * self.len()];
        for (item, chunk) in self.zip(output.chunks_mut(T::SERIALIZED_LEN)) {
            item.serialize_into(chunk)
        }

        output
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct E164(NonZeroU64);

impl E164 {
    pub const fn new(number: NonZeroU64) -> Self {
        Self(number)
    }

    fn from_serialized(bytes: [u8; E164::SERIALIZED_LEN]) -> Option<Self> {
        NonZeroU64::new(u64::from_be_bytes(bytes)).map(Self)
    }
}

impl From<E164> for NonZeroU64 {
    fn from(value: E164) -> Self {
        value.0
    }
}

impl FromStr for E164 {
    type Err = ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.strip_prefix('+').unwrap_or(s);
        NonZeroU64::from_str(s).map(Self)
    }
}

impl Display for E164 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "+{}", self.0)
    }
}

impl FixedLengthSerializable for E164 {
    const SERIALIZED_LEN: usize = 8;

    fn serialize_into(&self, target: &mut [u8]) {
        target.copy_from_slice(&self.0.get().to_be_bytes())
    }
}

pub struct AciAndAccessKey {
    pub aci: Aci,
    pub access_key: [u8; 16],
}

impl FixedLengthSerializable for AciAndAccessKey {
    const SERIALIZED_LEN: usize = 32;

    fn serialize_into(&self, target: &mut [u8]) {
        let uuid_bytes = Uuid::from(self.aci).into_bytes();

        target[0..uuid_bytes.len()].copy_from_slice(&uuid_bytes);
        target[uuid_bytes.len()..].copy_from_slice(&self.access_key);
    }
}

#[derive(Default)]
pub struct LookupRequest {
    pub new_e164s: Vec<E164>,
    pub prev_e164s: Vec<E164>,
    pub acis_and_access_keys: Vec<AciAndAccessKey>,
    pub return_acis_without_uaks: bool,
    pub token: Box<[u8]>,
}

impl LookupRequest {
    fn into_client_request(self) -> ClientRequest {
        let Self {
            new_e164s,
            prev_e164s,
            acis_and_access_keys,
            return_acis_without_uaks,
            token,
        } = self;

        let aci_uak_pairs = acis_and_access_keys.into_iter().collect_serialized();
        let new_e164s = new_e164s.into_iter().collect_serialized();
        let prev_e164s = prev_e164s.into_iter().collect_serialized();

        ClientRequest {
            aci_uak_pairs,
            new_e164s,
            prev_e164s,
            return_acis_without_uaks,
            token: token.into_vec(),
            token_ack: false,
            // TODO: use these for supporting non-desktop client requirements.
            discard_e164s: Vec::new(),
        }
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct Token(pub Box<[u8]>);

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct LookupResponse {
    pub records: Vec<LookupResponseEntry>,
    pub debug_permits_used: i32,
}

#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct LookupResponseEntry {
    pub e164: E164,
    pub aci: Option<Aci>,
    pub pni: Option<Pni>,
}

#[derive(Debug, PartialEq)]
pub enum LookupResponseParseError {
    InvalidNumberOfBytes { actual_length: usize },
}

impl From<LookupResponseParseError> for LookupError {
    fn from(value: LookupResponseParseError) -> Self {
        match value {
            LookupResponseParseError::InvalidNumberOfBytes { .. } => Self::ParseError,
        }
    }
}

impl TryFrom<ClientResponse> for LookupResponse {
    type Error = LookupResponseParseError;

    fn try_from(response: ClientResponse) -> Result<Self, Self::Error> {
        let ClientResponse {
            e164_pni_aci_triples,
            token: _,
            debug_permits_used,
        } = response;

        if e164_pni_aci_triples.len() % LookupResponseEntry::SERIALIZED_LEN != 0 {
            return Err(LookupResponseParseError::InvalidNumberOfBytes {
                actual_length: e164_pni_aci_triples.len(),
            });
        }

        let records = e164_pni_aci_triples
            .chunks(LookupResponseEntry::SERIALIZED_LEN)
            .flat_map(|record| {
                LookupResponseEntry::try_parse_from(
                    record.try_into().expect("chunk size is correct"),
                )
            })
            .collect();

        Ok(Self {
            records,
            debug_permits_used,
        })
    }
}

impl LookupResponseEntry {
    const UUID_LEN: usize = 16;
    const SERIALIZED_LEN: usize = E164::SERIALIZED_LEN + Self::UUID_LEN * 2;

    fn try_parse_from(record: &[u8; Self::SERIALIZED_LEN]) -> Option<Self> {
        fn non_nil_uuid<T: From<Uuid>>(bytes: &uuid::Bytes) -> Option<T> {
            let uuid = Uuid::from_bytes(*bytes);
            (!uuid.is_nil()).then(|| uuid.into())
        }

        // TODO(https://github.com/rust-lang/rust/issues/90091): use split_array
        // instead of expect() on the output.
        let (e164_bytes, record) = record.split_at(E164::SERIALIZED_LEN);
        let e164_bytes = <&[u8; E164::SERIALIZED_LEN]>::try_from(e164_bytes).expect("split at len");
        let e164 = E164::from_serialized(*e164_bytes)?;
        let (pni_bytes, aci_bytes) = record.split_at(Self::UUID_LEN);

        let pni = non_nil_uuid(pni_bytes.try_into().expect("split at len"));
        let aci = non_nil_uuid(aci_bytes.try_into().expect("split at len"));

        Some(Self { e164, aci, pni })
    }
}

pub struct CdsiConnection<S>(AttestedConnection<S>);

impl<S> AsMut<AttestedConnection<S>> for CdsiConnection<S> {
    fn as_mut(&mut self) -> &mut AttestedConnection<S> {
        &mut self.0
    }
}

/// Anything that can go wrong during a CDSI lookup.
#[derive(Debug, Error, displaydoc::Display)]
pub enum LookupError {
    /// Protocol error after establishing a connection.
    Protocol,
    /// SGX attestation failed.
    AttestationError(attest::enclave::Error),
    /// Invalid response received from the server.
    InvalidResponse,
    /// Retry later.
    RateLimited { retry_after_seconds: u32 },
    /// Failed to parse the response from the server.
    ParseError,
    /// Transport failed: {0}
    ConnectTransport(TransportConnectError),
    /// WebSocket error: {0}
    WebSocket(WebSocketServiceError),
    /// Lookup timed out
    Timeout,
}

impl From<AttestedConnectionError> for LookupError {
    fn from(value: AttestedConnectionError) -> Self {
        match value {
            AttestedConnectionError::ClientConnection(_) => Self::Protocol,
            AttestedConnectionError::WebSocket(e) => Self::WebSocket(e),
            AttestedConnectionError::Protocol => Self::Protocol,
            AttestedConnectionError::Sgx(e) => Self::AttestationError(e),
        }
    }
}

impl From<crate::enclave::Error> for LookupError {
    fn from(value: crate::enclave::Error) -> Self {
        match value {
            crate::svr::Error::WebSocketConnect(err) => match err {
                WebSocketConnectError::Timeout => Self::Timeout,
                WebSocketConnectError::Transport(e) => Self::ConnectTransport(e),
                WebSocketConnectError::WebSocketError(e) => Self::WebSocket(e.into()),
            },
            crate::svr::Error::AttestationError(err) => Self::AttestationError(err),
            crate::svr::Error::WebSocket(err) => Self::WebSocket(err),
            crate::svr::Error::Protocol => Self::Protocol,
            crate::svr::Error::Timeout => Self::Timeout,
        }
    }
}

impl From<prost::DecodeError> for LookupError {
    fn from(_value: prost::DecodeError) -> Self {
        Self::Protocol
    }
}

#[derive(serde::Deserialize)]
struct RateLimitExceededResponse {
    retry_after_seconds: u32,
}

impl RateLimitExceededResponse {
    /// Numeric code set by the server on the websocket close frame.
    const CLOSE_CODE: u16 = 4008;
}

pub struct ClientResponseCollector<S = SslStream<TcpStream>>(CdsiConnection<S>);

impl<S: AsyncDuplexStream> CdsiConnection<S> {
    /// Connect to remote host and verify remote attestation.
    pub async fn connect<C, T>(
        endpoint: &EnclaveEndpointConnection<Cdsi, C>,
        transport_connector: T,
        auth: impl HttpBasicAuth,
    ) -> Result<Self, LookupError>
    where
        C: ConnectionManager,
        T: TransportConnector<Stream = S>,
    {
        let connection = endpoint.connect(auth, transport_connector).await?;
        Ok(Self(connection))
    }

    pub async fn send_request(
        mut self,
        request: LookupRequest,
    ) -> Result<(Token, ClientResponseCollector<S>), LookupError> {
        self.0.send(request.into_client_request()).await?;
        let token_response: ClientResponse = match self.0.receive().await? {
            NextOrClose::Next(response) => response,
            NextOrClose::Close(close) => {
                if let Some(close) = close {
                    if u16::from(close.code) == RateLimitExceededResponse::CLOSE_CODE {
                        if let Ok(RateLimitExceededResponse {
                            retry_after_seconds,
                        }) = serde_json::from_str(&close.reason)
                        {
                            return Err(LookupError::RateLimited {
                                retry_after_seconds,
                            });
                        }
                    }
                };
                return Err(LookupError::Protocol);
            }
        };

        if token_response.token.is_empty() {
            return Err(LookupError::Protocol);
        }

        Ok((
            Token(token_response.token.into_boxed_slice()),
            ClientResponseCollector(self),
        ))
    }
}

impl<S: AsyncDuplexStream> ClientResponseCollector<S> {
    pub async fn collect(self) -> Result<LookupResponse, LookupError> {
        let Self(mut connection) = self;

        let token_ack = ClientRequest {
            token_ack: true,
            ..Default::default()
        };

        connection.0.send(token_ack).await?;
        let mut response: ClientResponse = connection
            .0
            .receive()
            .await?
            .next_or(LookupError::Protocol)?;
        while let NextOrClose::Next(decoded) = connection.0.receive_bytes().await? {
            response
                .merge(decoded.as_ref())
                .map_err(LookupError::from)?;
        }
        Ok(response.try_into()?)
    }
}

#[cfg(test)]
mod test {
    use hex_literal::hex;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn parse_lookup_response_entries() {
        const ACI_BYTES: [u8; 16] = hex!("0102030405060708a1a2a3a4a5a6a7a8");
        const PNI_BYTES: [u8; 16] = hex!("b1b2b3b4b5b6b7b81112131415161718");

        let e164: E164 = "+18005551001".parse().unwrap();
        let mut e164_bytes = [0; 8];
        e164.serialize_into(&mut e164_bytes);

        // Generate a sequence of triples by repeating the above data a few times.
        const NUM_REPEATS: usize = 4;
        let e164_pni_aci_triples =
            std::iter::repeat([e164_bytes.as_slice(), &PNI_BYTES, &ACI_BYTES])
                .take(NUM_REPEATS)
                .flatten()
                .flatten()
                .cloned()
                .collect();

        let parsed = ClientResponse {
            e164_pni_aci_triples,
            token: vec![],
            debug_permits_used: 42,
        }
        .try_into();
        assert_eq!(
            parsed,
            Ok(LookupResponse {
                records: vec![
                    LookupResponseEntry {
                        e164,
                        aci: Some(Aci::from(Uuid::from_bytes(ACI_BYTES))),
                        pni: Some(Pni::from(Uuid::from_bytes(PNI_BYTES))),
                    };
                    NUM_REPEATS
                ],
                debug_permits_used: 42
            })
        );
    }

    #[test]
    fn serialize_e164s() {
        let e164s: Vec<E164> = (18005551001..)
            .take(5)
            .map(|n| E164(NonZeroU64::new(n).unwrap()))
            .collect();
        let serialized = e164s.into_iter().collect_serialized();

        assert_eq!(
            serialized.as_slice(),
            &hex!(
                "000000043136e799"
                "000000043136e79a"
                "000000043136e79b"
                "000000043136e79c"
                "000000043136e79d"
            )
        );
    }

    #[test]
    fn serialize_acis_and_access_keys() {
        let pairs = [1, 2, 3, 4, 5].map(|i| AciAndAccessKey {
            access_key: [i; 16],
            aci: Aci::from_uuid_bytes([i | 0x80; 16]),
        });
        let serialized = pairs.into_iter().collect_serialized();

        assert_eq!(
            serialized.as_slice(),
            &hex!(
                "8181818181818181818181818181818101010101010101010101010101010101"
                "8282828282828282828282828282828202020202020202020202020202020202"
                "8383838383838383838383838383838303030303030303030303030303030303"
                "8484848484848484848484848484848404040404040404040404040404040404"
                "8585858585858585858585858585858505050505050505050505050505050505"
            )
        );
    }
}
