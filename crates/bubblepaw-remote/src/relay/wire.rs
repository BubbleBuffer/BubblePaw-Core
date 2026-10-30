use super::Limits;
use crate::{Error, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

pub(super) const REGISTER: u8 = 2;
pub(super) const CONNECT: u8 = 3;

#[derive(Clone)]
pub(super) enum Wire {
    Challenge([u8; 32]),
    Auth {
        role: u8,
        key: [u8; 32],
        signature: [u8; 64],
        target: String,
    },
    Registered,
    Incoming(u64, [u8; 32]),
    Accept(u64),
    Data(u64, Vec<u8>),
    Close(u64),
    Connected(u64, [u8; 32]),
    Reject,
}

pub(super) fn signed(challenge: &[u8; 32], role: u8, target: &str) -> Vec<u8> {
    let mut bytes = b"bubblepaw-relay-v1\0".to_vec();
    bytes.extend_from_slice(challenge);
    bytes.push(role);
    bytes.extend_from_slice(target.as_bytes());
    bytes
}

pub(super) fn config(limits: &Limits) -> WebSocketConfig {
    WebSocketConfig::default()
        .write_buffer_size(0)
        .max_write_buffer_size(limits.max_frame_bytes.saturating_add(512))
        .max_message_size(Some(limits.max_frame_bytes.saturating_add(256)))
        .max_frame_size(Some(limits.max_frame_bytes.saturating_add(256)))
}

pub(super) fn validate_config<S>(ws: &WebSocketStream<S>, limits: &Limits) -> Result<()> {
    let config = ws.get_config();
    let cap = limits.max_frame_bytes.saturating_add(256);
    if config.max_frame_size.is_none_or(|size| size > cap)
        || config.max_message_size.is_none_or(|size| size > cap)
        || config.max_write_buffer_size > limits.max_frame_bytes.saturating_add(512)
    {
        return Err(Error::Protocol(
            "WebSocket requires bounded relay configuration",
        ));
    }
    Ok(())
}

impl Wire {
    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Challenge(value) => [vec![1], value.to_vec()].concat(),
            Self::Auth {
                role,
                key,
                signature,
                target,
            } => [
                vec![*role],
                key.to_vec(),
                signature.to_vec(),
                target.as_bytes().to_vec(),
            ]
            .concat(),
            Self::Registered => vec![4],
            Self::Incoming(id, key) => [vec![5], id.to_be_bytes().to_vec(), key.to_vec()].concat(),
            Self::Accept(id) => [vec![6], id.to_be_bytes().to_vec()].concat(),
            Self::Data(id, data) => [vec![7], id.to_be_bytes().to_vec(), data.clone()].concat(),
            Self::Close(id) => [vec![8], id.to_be_bytes().to_vec()].concat(),
            Self::Connected(id, key) => [vec![9], id.to_be_bytes().to_vec(), key.to_vec()].concat(),
            Self::Reject => vec![10],
        }
    }

    fn decode(bytes: &[u8], limits: &Limits) -> Result<Self> {
        let invalid = || Error::Protocol("invalid relay frame");
        let op = *bytes.first().ok_or_else(invalid)?;
        let sid = || -> Result<u64> {
            Ok(u64::from_be_bytes(
                bytes
                    .get(1..9)
                    .ok_or_else(invalid)?
                    .try_into()
                    .map_err(|_| invalid())?,
            ))
        };
        match op {
            1 if bytes.len() == 33 => Ok(Self::Challenge(
                bytes[1..].try_into().map_err(|_| invalid())?,
            )),
            REGISTER | CONNECT if (97..=225).contains(&bytes.len()) => Ok(Self::Auth {
                role: op,
                key: bytes[1..33].try_into().map_err(|_| invalid())?,
                signature: bytes[33..97].try_into().map_err(|_| invalid())?,
                target: std::str::from_utf8(&bytes[97..])
                    .map_err(|_| invalid())?
                    .to_owned(),
            }),
            4 if bytes.len() == 1 => Ok(Self::Registered),
            5 | 9 if bytes.len() == 41 => {
                let key = bytes[9..].try_into().map_err(|_| invalid())?;
                if op == 5 {
                    Ok(Self::Incoming(sid()?, key))
                } else {
                    Ok(Self::Connected(sid()?, key))
                }
            }
            6 if bytes.len() == 9 => Ok(Self::Accept(sid()?)),
            7 if bytes.len() >= 9 => {
                if bytes.len() - 9 > limits.max_frame_bytes {
                    return Err(Error::OversizedFrame);
                }
                Ok(Self::Data(sid()?, bytes[9..].to_vec()))
            }
            8 if bytes.len() == 9 => Ok(Self::Close(sid()?)),
            10 if bytes.len() == 1 => Ok(Self::Reject),
            _ => Err(invalid()),
        }
    }
}

pub(super) async fn send<S>(
    ws: &mut WebSocketStream<S>,
    value: &Wire,
    limits: &Limits,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(
        limits.idle_timeout,
        ws.send(Message::Binary(value.encode().into())),
    )
    .await
    .map_err(|_| Error::IdleTimeout)??;
    Ok(())
}

pub(super) async fn recv<S>(ws: &mut WebSocketStream<S>, limits: &Limits) -> Result<Wire>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(limits.idle_timeout, async {
        loop {
            match ws.next().await.ok_or(Error::Closed)?? {
                Message::Binary(bytes) => return Wire::decode(&bytes, limits),
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => return Err(Error::Closed),
                _ => return Err(Error::Protocol("relay requires binary frames")),
            }
        }
    })
    .await
    .map_err(|_| Error::IdleTimeout)?
}
