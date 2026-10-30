use std::{sync::Arc, time::Duration};

use bubblepaw_remote::{
    Error, Result,
    device::{DeviceId, DeviceKey},
    frame::{ByteFrames, FrameIo, MAX_FRAME_LEN},
    store::{PairStore, PairedClient, PairedHost, Profile},
    trust::{DeviceListAuthorizer, PeerAuthorizer, PublicKey},
    tunnel::{self, Side, Tunnel},
};
use tokio::io::{DuplexStream, duplex};

type ByteTunnel = Tunnel<ByteFrames<DuplexStream>>;

fn entry(key: &DeviceKey, profile: Profile) -> (DeviceId, PublicKey, Profile) {
    (key.id(), key.public_key(), profile)
}

async fn admit(
    alice: &DeviceKey,
    bob: &DeviceKey,
    alice_policy: &dyn PeerAuthorizer,
    bob_policy: &dyn PeerAuthorizer,
) -> (Result<ByteTunnel>, Result<ByteTunnel>) {
    let (left, right) = duplex(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            tunnel::connect(ByteFrames::new(left), alice, alice_policy),
            tunnel::accept(ByteFrames::new(right), bob, bob_policy),
        )
    })
    .await
    .expect("admission must complete on a single-byte carrier")
}

#[test]
fn invalid_device_list_replacements_preserve_the_previous_complete_policy() {
    let alice = DeviceKey::from_seed(&[31; 32]);
    let bob = DeviceKey::from_seed(&[32; 32]);
    let policy = DeviceListAuthorizer::new([entry(&alice, Profile::Read)]).unwrap();
    let replacement = [
        entry(&alice, Profile::Edit),
        (bob.id(), alice.public_key(), Profile::Full),
    ];
    assert!(policy.replace(replacement).is_err());
    assert_eq!(
        policy.authorize(&alice.id(), &alice.public_key()),
        Some(Profile::Read)
    );
    assert_eq!(policy.authorize(&bob.id(), &bob.public_key()), None);
    assert!(
        policy
            .replace([entry(&alice, Profile::Edit), entry(&alice, Profile::Full)])
            .is_err()
    );
    let invalid_public = [0; 32];
    assert!(
        policy
            .replace([(
                DeviceId::from_public_key(&invalid_public),
                invalid_public,
                Profile::Full
            )])
            .is_err()
    );
    assert_eq!(
        policy.authorize(&alice.id(), &alice.public_key()),
        Some(Profile::Read)
    );
    assert!(DeviceListAuthorizer::new([(bob.id(), alice.public_key(), Profile::Read)]).is_err());
    assert!(
        DeviceListAuthorizer::new([entry(&bob, Profile::Read), entry(&bob, Profile::Edit)])
            .is_err()
    );
}

#[test]
fn cloned_device_lists_share_swaps_and_require_both_id_and_exact_key() {
    let alice = DeviceKey::from_seed(&[33; 32]);
    let bob = DeviceKey::from_seed(&[34; 32]);
    let policy = DeviceListAuthorizer::new([entry(&alice, Profile::Read)]).unwrap();
    let shared = policy.clone();
    assert_eq!(shared.authorize(&alice.id(), &bob.public_key()), None);
    assert_eq!(shared.authorize(&bob.id(), &alice.public_key()), None);
    policy
        .replace([entry(&alice, Profile::Edit), entry(&bob, Profile::Read)])
        .unwrap();
    assert_eq!(
        shared.authorize(&alice.id(), &alice.public_key()),
        Some(Profile::Edit)
    );
    assert_eq!(
        shared.authorize(&bob.id(), &bob.public_key()),
        Some(Profile::Read)
    );
    shared.replace([]).unwrap();
    assert_eq!(policy.authorize(&alice.id(), &alice.public_key()), None);
}

#[test]
fn policy_readers_never_observe_an_intermediate_empty_list_during_replacement() {
    let key = DeviceKey::from_seed(&[35; 32]);
    let id = key.id();
    let public = key.public_key();
    let policy = Arc::new(DeviceListAuthorizer::new([entry(&key, Profile::Read)]).unwrap());
    let writer = Arc::clone(&policy);
    let task = std::thread::spawn(move || {
        for index in 0..64 {
            let profile = if index % 2 == 0 {
                Profile::Edit
            } else {
                Profile::Read
            };
            writer.replace([(id, public, profile)]).unwrap();
        }
    });
    for _ in 0..1024 {
        assert!(matches!(
            policy.authorize(&id, &public),
            Some(Profile::Read | Profile::Edit)
        ));
    }
    task.join().unwrap();
}

#[test]
fn json_stores_authorize_exact_id_and_key_using_the_saved_profile() {
    let directory = tempfile::tempdir().unwrap();
    let key = DeviceKey::from_seed(&[36; 32]);
    let other = DeviceKey::from_seed(&[37; 32]);
    let path = directory.path().join("clients.json");
    let mut clients = PairStore::<PairedClient>::open(&path).unwrap();
    clients
        .insert(PairedClient {
            public_key: key.public_key(),
            name: "Phone".into(),
            profile: Profile::Edit,
        })
        .unwrap();
    let mut loaded = PairStore::<PairedClient>::open(path).unwrap();
    assert_eq!(
        loaded.authorize(&key.id(), &key.public_key()),
        Some(Profile::Edit)
    );
    assert_eq!(loaded.authorize(&key.id(), &other.public_key()), None);
    assert_eq!(loaded.authorize(&other.id(), &key.public_key()), None);
    assert!(loaded.revoke(&key.id()).unwrap());
    assert_eq!(loaded.authorize(&key.id(), &key.public_key()), None);
    let mut hosts = PairStore::<PairedHost>::open(directory.path().join("hosts.json")).unwrap();
    hosts
        .insert(PairedHost {
            id: key.id().to_string(),
            public_key: key.public_key(),
            name: "Host".into(),
            reach_address: "ws://example.test".into(),
        })
        .unwrap();
    assert_eq!(
        hosts.authorize(&key.id(), &key.public_key()),
        Some(Profile::Full)
    );
    assert_eq!(hosts.authorize(&key.id(), &other.public_key()), None);
}

#[tokio::test]
async fn supplied_device_lists_admit_without_pairing_and_swaps_reject_new_connections() {
    let alice = DeviceKey::from_seed(&[38; 32]);
    let bob = DeviceKey::from_seed(&[39; 32]);
    let alice_policy = DeviceListAuthorizer::new([entry(&bob, Profile::Edit)]).unwrap();
    let bob_policy = DeviceListAuthorizer::new([entry(&alice, Profile::Read)]).unwrap();
    let shared_bob_policy = bob_policy.clone();
    let (left, right) = admit(&alice, &bob, &alice_policy, &shared_bob_policy).await;
    let mut left = left.unwrap();
    let mut right = right.unwrap();
    let grant = right.authorization().unwrap();
    assert_eq!(grant.id, alice.id());
    assert_eq!(grant.public_key, alice.public_key());
    assert_eq!(grant.profile, Profile::Read);
    assert_eq!(left.authorization().unwrap().profile, Profile::Edit);
    bob_policy.replace([]).unwrap();
    let (new_left, new_right) = admit(&alice, &bob, &alice_policy, &shared_bob_policy).await;
    assert!(matches!(new_left, Err(Error::UnpairedPeer)));
    assert!(matches!(new_right, Err(Error::UnpairedPeer)));
    assert_eq!(right.authorization().unwrap().profile, Profile::Read);
    let (sent, received) = tokio::join!(left.send(b"existing grant is a snapshot"), right.recv());
    sent.unwrap();
    assert_eq!(received.unwrap(), b"existing grant is a snapshot");
}

#[tokio::test]
async fn json_policy_is_used_by_the_tunnel_and_exposes_its_grant() {
    let alice = DeviceKey::from_seed(&[40; 32]);
    let bob = DeviceKey::from_seed(&[41; 32]);
    let directory = tempfile::tempdir().unwrap();
    let mut clients =
        PairStore::<PairedClient>::open(directory.path().join("clients.json")).unwrap();
    clients
        .insert(PairedClient {
            public_key: alice.public_key(),
            name: "Preconfigured".into(),
            profile: Profile::Read,
        })
        .unwrap();
    let alice_trust = [bob.public_key()];
    let (left, right) = admit(&alice, &bob, &alice_trust, &clients).await;
    assert_eq!(
        left.unwrap().authorization().unwrap().profile,
        Profile::Full
    );
    assert_eq!(
        right.unwrap().authorization().unwrap().profile,
        Profile::Read
    );
}

#[tokio::test]
async fn forged_ed_identity_is_rejected_before_the_authorizer_is_called() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingPolicy(AtomicUsize);
    impl PeerAuthorizer for CountingPolicy {
        fn authorize(&self, _: &DeviceId, _: &PublicKey) -> Option<Profile> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Some(Profile::Full)
        }
    }
    let alice = DeviceKey::from_seed(&[42; 32]);
    let bob = DeviceKey::from_seed(&[43; 32]);
    let forged = DeviceKey::from_seed(&[44; 32]);
    let policy = CountingPolicy(AtomicUsize::new(0));
    let (left, right) = duplex(1024);
    let malicious = async {
        let mut io = ByteFrames::new(left);
        let mut result = tunnel::exchange(&mut io, &alice, Side::Initiator, None)
            .await
            .unwrap();
        let mut buffer = vec![0; MAX_FRAME_LEN];
        let host_identity = io.recv_frame().await.unwrap();
        let length = result
            .transport
            .read_message(&host_identity, &mut buffer)
            .unwrap();
        assert_eq!(&buffer[..length], &bob.public_key());
        let length = result
            .transport
            .write_message(&forged.public_key(), &mut buffer)
            .unwrap();
        io.send_frame(&buffer[..length]).await.unwrap();
    };
    let (_, admitted) = tokio::join!(
        malicious,
        tunnel::accept(ByteFrames::new(right), &bob, &policy)
    );
    assert!(matches!(admitted, Err(Error::UnpairedPeer)));
    assert_eq!(policy.0.load(Ordering::SeqCst), 0);
}
