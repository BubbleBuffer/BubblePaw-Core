//! Host-side pairing: one active offer at a time, holding a one-time code.
//! A device that proves the code becomes a [`PairingRequest`] the person at
//! the host confirms (after comparing the check number) or declines.
use super::{PairedDevice, PairingPayload, RemoteAddress, paired_profile};
use crate::host_api::Profile;
use anyhow::{Result, anyhow};
use bubblepaw_remote::device::{DeviceId, DeviceKey};
use bubblepaw_remote::frame::FrameIo;
use bubblepaw_remote::pairing::{self, PairingCode, PendingHost};
use bubblepaw_remote::store::{DeviceKind, PairStore, PairedClient};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, oneshot};

/// How long one attempt may hold the offer, so a stalled peer cannot keep
/// others from trying until the code expires.
const ATTEMPT_LIMIT: Duration = Duration::from_secs(60);

/// The offer slot shared by a server and its pairing sessions.
pub(super) type OfferSlot = Arc<Mutex<Option<ActiveOffer>>>;

pub(super) struct ActiveOffer {
    id: u64,
    code: PairingCode,
    profile: Profile,
    egress: bool,
    requests: mpsc::Sender<PairingRequest>,
}

/// A pairing offer: show [`Self::code`] or [`Self::qr_payload`] to the person
/// pairing a device, then wait for [`Self::next`]. Dropping the offer withdraws
/// it. The code is a secret: never log it.
pub struct PairingOffer {
    id: u64,
    slot: OfferSlot,
    payload: PairingPayload,
    deadline: Instant,
    requests: mpsc::Receiver<PairingRequest>,
}

impl std::fmt::Debug for PairingOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingOffer")
            .field("payload", &self.payload)
            .finish_non_exhaustive()
    }
}

impl PairingOffer {
    /// Install a new offer in `slot`, replacing any earlier one.
    pub(super) async fn install(
        slot: &OfferSlot,
        id: u64,
        address: &RemoteAddress,
        host: &DeviceId,
        profile: Profile,
        egress: bool,
    ) -> Result<Self> {
        let code = PairingCode::generate().map_err(|error| anyhow!("{error}"))?;
        let payload = PairingPayload::new(address, host.to_string(), code.code().to_owned());
        let deadline = Instant::now() + super::PAIRING_WINDOW;
        let (requests, receiver) = mpsc::channel(1);
        *slot.lock().await = Some(ActiveOffer {
            id,
            code,
            profile,
            egress,
            requests,
        });
        Ok(Self {
            id,
            slot: slot.clone(),
            payload,
            deadline,
            requests: receiver,
        })
    }

    /// The 10-character code to type on the device.
    pub fn code(&self) -> &str {
        &self.payload.code
    }

    /// What the QR code carries (see [`PairingPayload`]).
    pub fn qr_payload(&self) -> &PairingPayload {
        &self.payload
    }

    /// Time left before the code expires.
    pub fn expires_in(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// The device that proved the code, or `None` once the offer expired.
    /// A wrong code does not end the offer; the device can try again.
    pub async fn next(&mut self) -> Option<PairingRequest> {
        tokio::time::timeout_at(self.deadline.into(), self.requests.recv())
            .await
            .ok()
            .flatten()
    }
}

impl Drop for PairingOffer {
    fn drop(&mut self) {
        // Best effort: an attempt holding the slot finds the receiver gone.
        if let Ok(mut slot) = self.slot.try_lock()
            && slot.as_ref().is_some_and(|offer| offer.id == self.id)
        {
            *slot = None;
        }
    }
}

enum Answer {
    Approve {
        name: Option<String>,
        done: oneshot::Sender<Result<PairedDevice>>,
    },
    Decline,
}

/// A device that proved the pairing code. Show [`Self::comparison`] and
/// approve only when the device shows the same number.
pub struct PairingRequest {
    comparison: String,
    device_name: String,
    device_id: String,
    device_kind: DeviceKind,
    answer: oneshot::Sender<Answer>,
}

impl std::fmt::Debug for PairingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingRequest")
            .field("device_name", &self.device_name)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl PairingRequest {
    /// The 6-digit check number both sides show.
    pub fn comparison(&self) -> &str {
        &self.comparison
    }

    /// The name the device suggested for itself.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// What the device says it is (`Other` when it did not say).
    pub fn device_kind(&self) -> DeviceKind {
        self.device_kind
    }

    /// Store the device with the offer's profile and egress grant, under `name` or the name it
    /// suggested, and tell it the pairing succeeded.
    pub async fn approve(self, name: Option<String>) -> Result<PairedDevice> {
        let (done, result) = oneshot::channel();
        self.answer
            .send(Answer::Approve { name, done })
            .map_err(|_| anyhow!("the pairing attempt ended"))?;
        result
            .await
            .map_err(|_| anyhow!("the pairing attempt ended"))?
    }

    /// Tell the device the pairing was declined; nothing is stored.
    pub fn decline(self) {
        let _ = self.answer.send(Answer::Decline);
    }
}

/// A code that is already past its deadline, so a device that asks to pair
/// while no offer is open learns that its code expired.
fn expired_code() -> Option<PairingCode> {
    let issued = Instant::now().checked_sub(super::PAIRING_WINDOW + Duration::from_secs(1))?;
    PairingCode::generate_at(issued).ok()
}

/// Run one pairing attempt on `session`. Returns once the device is stored,
/// declined, or the attempt failed. `store` serializes paired-store writes.
pub(super) async fn attempt<T: FrameIo>(
    slot: &OfferSlot,
    session: T,
    key: &DeviceKey,
    host_name: &str,
    clients: &Path,
    store: &Mutex<()>,
) -> Result<bool> {
    let mut guard = slot.lock().await;
    let open = guard
        .as_ref()
        .is_some_and(|offer| !offer.code.expired() && !offer.requests.is_closed());
    if !open {
        *guard = None;
        drop(guard);
        if let Some(mut code) = expired_code() {
            let _ = pairing::accept(session, key, &mut code, host_name).await;
        }
        return Ok(false);
    }
    let offer = guard.as_mut().expect("checked above");
    let accepted = tokio::time::timeout(
        ATTEMPT_LIMIT,
        pairing::accept(session, key, &mut offer.code, host_name),
    )
    .await;
    let pending = match accepted {
        Ok(Ok(pending)) => pending,
        // A wrong code leaves the offer open for another try.
        Ok(Err(bubblepaw_remote::Error::WrongCode)) => return Ok(false),
        Ok(Err(error)) => {
            if offer.code.used() || offer.code.expired() {
                *guard = None;
            }
            return Err(anyhow!("pairing attempt failed: {error}"));
        }
        Err(_) => {
            if offer.code.used() {
                *guard = None;
            }
            return Err(anyhow!("pairing attempt timed out"));
        }
    };
    // The code is used now; later attempts see it as expired.
    let profile = offer.profile;
    let egress = offer.egress;
    let requests = offer.requests.clone();
    *guard = None;
    drop(guard);
    decide(pending, profile, egress, requests, clients, store).await
}

async fn decide<T: FrameIo>(
    pending: PendingHost<T>,
    profile: Profile,
    egress: bool,
    requests: mpsc::Sender<PairingRequest>,
    clients: &Path,
    store: &Mutex<()>,
) -> Result<bool> {
    let (answer, answered) = oneshot::channel();
    let request = PairingRequest {
        comparison: pending.comparison().to_owned(),
        device_name: pending.client_suggested_name().to_owned(),
        device_id: DeviceId::from_public_key(pending.client_public_key()).to_string(),
        device_kind: pending.client_kind(),
        answer,
    };
    if requests.send(request).await.is_err() {
        let _ = pending.decline().await;
        return Ok(false);
    }
    let Ok(Answer::Approve { name, done }) = answered.await else {
        let _ = pending.decline().await;
        return Ok(false);
    };
    let name = name.unwrap_or_else(|| pending.client_suggested_name().to_owned());
    let public_key = *pending.client_public_key();
    let result = async {
        let _writing = store.lock().await;
        let mut clients_store = PairStore::<PairedClient>::open(clients)
            .map_err(|error| anyhow!("could not read the paired-device store: {error}"))?;
        pending
            .approve_with_egress(
                name.clone(),
                paired_profile(profile),
                egress,
                &mut clients_store,
            )
            .await
            .map_err(|error| anyhow!("pairing failed: {error}"))?;
        let id = DeviceId::from_public_key(&public_key);
        let stored = clients_store.get(&id);
        Ok(PairedDevice {
            id: id.to_string(),
            label: name,
            profile,
            paired_at: stored.and_then(|client| client.paired_at),
            kind: stored.map(|client| client.kind).unwrap_or_default(),
            last_seen: None,
            egress,
        })
    }
    .await;
    let paired = result.is_ok();
    let _ = done.send(result);
    Ok(paired)
}
