use super::{
    Limits,
    server::CiphertextObserver,
    wire::{self, Wire},
};
use crate::{Error, Result, device::DeviceId};
use std::collections::HashMap;
use tokio::sync::mpsc;

struct Client {
    connection: u64,
    sender: mpsc::Sender<Wire>,
    accepted: bool,
}
struct Host {
    connection: u64,
    key: [u8; 32],
    sender: mpsc::Sender<Wire>,
    clients: HashMap<u64, Client>,
}
#[derive(Default)]
pub(super) struct State {
    hosts: HashMap<String, Host>,
    next_session: u64,
}
pub(super) enum Role {
    Host(String),
    Client(String, u64),
}

fn enqueue(sender: &mpsc::Sender<Wire>, frame: Wire) -> Result<()> {
    sender
        .try_send(frame)
        .map_err(|_| Error::Limit("outbound queue"))
}

impl State {
    pub(super) fn attach(
        &mut self,
        role: u8,
        key: [u8; 32],
        target: String,
        connection: u64,
        sender: mpsc::Sender<Wire>,
        limits: &Limits,
    ) -> Result<Role> {
        if role == wire::REGISTER {
            let id = DeviceId::from_public_key(&key).to_string();
            if !target.is_empty() || self.hosts.contains_key(&id) {
                return Err(Error::AuthenticationFailed);
            }
            self.hosts.insert(
                id.clone(),
                Host {
                    connection,
                    key,
                    sender,
                    clients: HashMap::new(),
                },
            );
            return Ok(Role::Host(id));
        }
        DeviceId::parse(&target).map_err(|_| Error::AuthenticationFailed)?;
        let host = self.hosts.get_mut(&target).ok_or(Error::Closed)?;
        if host.clients.len() >= limits.sessions_per_host {
            return Err(Error::Limit("host sessions"));
        }
        if host
            .clients
            .values()
            .filter(|client| !client.accepted)
            .count()
            >= limits.pending_per_host
        {
            return Err(Error::Limit("pending clients"));
        }
        self.next_session = self
            .next_session
            .checked_add(1)
            .ok_or(Error::Limit("session ids"))?;
        let session = self.next_session;
        enqueue(&host.sender, Wire::Incoming(session, key))?;
        host.clients.insert(
            session,
            Client {
                connection,
                sender,
                accepted: false,
            },
        );
        Ok(Role::Client(target, session))
    }

    pub(super) fn route(
        &mut self,
        role: &Role,
        frame: Wire,
        observer: &Option<CiphertextObserver>,
    ) -> Result<()> {
        match role {
            Role::Host(id) => {
                let host = self.hosts.get_mut(id).ok_or(Error::Closed)?;
                match frame {
                    Wire::Accept(session) => {
                        let Some(client) = host.clients.get_mut(&session) else {
                            return Ok(());
                        };
                        if client.accepted {
                            return Ok(());
                        }
                        enqueue(&client.sender, Wire::Connected(session, host.key))?;
                        client.accepted = true;
                    }
                    Wire::Data(session, bytes) => {
                        let Some(client) = host.clients.get(&session).filter(|c| c.accepted) else {
                            return Ok(());
                        };
                        if let Some(observe) = observer {
                            observe(&bytes);
                        }
                        enqueue(&client.sender, Wire::Data(session, bytes))?;
                    }
                    Wire::Close(session) => {
                        if let Some(client) = host.clients.remove(&session) {
                            let _ = enqueue(&client.sender, Wire::Close(session));
                        }
                    }
                    _ => return Err(Error::Protocol("invalid host command")),
                }
            }
            Role::Client(id, session) => {
                let host = self.hosts.get_mut(id).ok_or(Error::Closed)?;
                if !host
                    .clients
                    .get(session)
                    .is_some_and(|client| client.accepted)
                {
                    return Err(Error::Closed);
                }
                match frame {
                    Wire::Data(received, bytes) if received == *session => {
                        if let Some(observe) = observer {
                            observe(&bytes);
                        }
                        enqueue(&host.sender, Wire::Data(*session, bytes))?;
                    }
                    Wire::Close(received) if received == *session => {
                        return Err(Error::Closed);
                    }
                    _ => return Err(Error::Protocol("invalid client command")),
                }
            }
        }
        Ok(())
    }

    pub(super) fn detach(&mut self, role: &Role, connection: u64) {
        match role {
            Role::Host(id) => {
                if self
                    .hosts
                    .get(id)
                    .is_some_and(|h| h.connection == connection)
                    && let Some(host) = self.hosts.remove(id)
                {
                    for (session, client) in host.clients {
                        let _ = enqueue(&client.sender, Wire::Close(session));
                    }
                }
            }
            Role::Client(id, session) => {
                if let Some(host) = self.hosts.get_mut(id)
                    && host
                        .clients
                        .get(session)
                        .is_some_and(|c| c.connection == connection)
                {
                    host.clients.remove(session);
                    let _ = enqueue(&host.sender, Wire::Close(*session));
                }
            }
        }
    }
}
