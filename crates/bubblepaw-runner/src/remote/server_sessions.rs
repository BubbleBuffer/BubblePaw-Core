//! The sessions the server serves once admitted: a host-API tunnel and a model-traffic forwarding session.
use super::*;
use crate::remote::egress::{self, Refusal};

/// Serve one egress session: a paired device asks for a connection to a model
/// provider and this host copies the (end-to-end encrypted) bytes. Admission is
/// the same as for a host-API tunnel; the egress grant is checked after it, so
/// a device without the grant learns why instead of seeing a dropped tunnel.
pub(super) async fn serve_egress<T: SplitFrames + 'static>(
    inner: &Arc<Inner>,
    session: T,
) -> Result<()> {
    inner.refresh(false)?;
    let tunnel = tokio::time::timeout(
        ADMIT_WAIT,
        tunnel::accept(session, &inner.key, &inner.authorizer),
    )
    .await
    .context("tunnel admission timed out")?
    .map_err(|error| anyhow!("tunnel refused: {error}"))?;
    let peer = tunnel
        .authorization()
        .cloned()
        .context("tunnel admitted without a grant")?;
    let device = peer.id.to_string();
    let granted = inner.egress_ids.lock().unwrap().contains(&device);
    let slot = inner.egress_slots.clone().try_acquire_owned();
    let gate = if !granted {
        Err(Refusal::new(
            "not_granted",
            "this host has not allowed this device to send model traffic through it",
        ))
    } else if slot.is_err() {
        Err(Refusal::new(
            "busy",
            "this host is forwarding as much as it allows",
        ))
    } else {
        Ok(())
    };
    let (reader, writer) = tunnel.split().map_err(|error| anyhow!("{error}"))?;
    let stop = CancellationToken::new();
    let id = inner.next.fetch_add(1, Ordering::Relaxed);
    inner.tunnels.lock().unwrap().insert(
        id,
        Live {
            device,
            profile: api_profile(peer.profile),
            egress: granted,
            stop: stop.clone(),
        },
    );
    // A revoke or shutdown between admission and registration is caught here.
    inner.refresh(true).ok();
    if inner.stop.is_cancelled() {
        stop.cancel();
    }
    egress::serve_forward(
        reader,
        writer,
        gate,
        inner.egress_dial.as_ref(),
        stop.clone(),
    )
    .await;
    stop.cancel();
    inner.tunnels.lock().unwrap().remove(&id);
    drop(slot);
    Ok(())
}

pub(super) async fn serve_tunnel<T: SplitFrames + 'static>(
    inner: &Arc<Inner>,
    session: T,
) -> Result<()> {
    // Admit against the store as it is now, not as of the last poll.
    inner.refresh(false)?;
    let tunnel = tokio::time::timeout(
        ADMIT_WAIT,
        tunnel::accept(session, &inner.key, &inner.authorizer),
    )
    .await
    .context("tunnel admission timed out")?
    .map_err(|error| anyhow!("tunnel refused: {error}"))?;
    let peer = tunnel
        .authorization()
        .cloned()
        .context("tunnel admitted without a grant")?;
    inner.touch_last_seen(&peer.id).await;
    let Some(backend) = inner.backend.clone() else {
        return Ok(());
    };
    let profile = api_profile(peer.profile);
    let stop = CancellationToken::new();
    let id = inner.next.fetch_add(1, Ordering::Relaxed);
    inner.tunnels.lock().unwrap().insert(
        id,
        Live {
            device: peer.id.to_string(),
            profile,
            egress: false,
            stop: stop.clone(),
        },
    );
    // A revoke or shutdown between admission and registration is caught here.
    inner.refresh(true).ok();
    if inner.stop.is_cancelled() {
        stop.cancel();
    }
    let result = async {
        let (reader, writer) = tunnel.split().map_err(|error| anyhow!("{error}"))?;
        match &backend {
            SessionBackend::Context(ctx) => {
                adapter::serve_in_process(ctx, profile, reader, writer, stop.clone()).await
            }
            SessionBackend::Attach(data_dir) => {
                adapter::serve_attached(data_dir, profile, reader, writer, stop.clone()).await
            }
        }
    }
    .await;
    stop.cancel();
    inner.tunnels.lock().unwrap().remove(&id);
    result
}
