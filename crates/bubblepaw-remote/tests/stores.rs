use bubblepaw_remote::{
    device::DeviceKey,
    store::{PairStore, PairedClient, PairedHost, Profile},
};

#[test]
fn stores_validate_and_revoke_with_atomic_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clients.json");
    let mut store = PairStore::<PairedClient>::open(&path).unwrap();
    let key = DeviceKey::from_seed(&[7; 32]);
    store
        .insert(PairedClient {
            public_key: key.public_key(),
            name: "Phone".into(),
            profile: Profile::Read,
        })
        .unwrap();
    store
        .insert(PairedClient {
            public_key: key.public_key(),
            name: "Renamed".into(),
            profile: Profile::Full,
        })
        .unwrap();
    assert_eq!(store.devices().count(), 1);
    let original = std::fs::read(&path).unwrap();
    assert!(
        store
            .insert(PairedClient {
                public_key: [0; 32],
                name: "bad key".into(),
                profile: Profile::Read
            })
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let mut reopened = PairStore::<PairedClient>::open(&path).unwrap();
    assert_eq!(reopened.get(&key.id()).unwrap().name, "Renamed");
    assert_eq!(reopened.public_keys(), [key.public_key()]);
    assert!(reopened.revoke(&key.id()).unwrap());
    assert_eq!(
        PairStore::<PairedClient>::open(&path)
            .unwrap()
            .devices()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no temporary files left behind"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn corrupt_or_misidentified_stores_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hosts.json");
    std::fs::write(&path, b"{").unwrap();
    assert!(PairStore::<PairedHost>::open(&path).is_err());
    let key = DeviceKey::from_seed(&[8; 32]);
    let host = PairedHost {
        id: key.id().to_string(),
        public_key: key.public_key(),
        name: "Desktop".into(),
        reach_address: "ws://relay.test/".into(),
    };
    let value = serde_json::json!({"version":1,"devices":{"0000-0000-0000-0000":host}});
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(PairStore::<PairedHost>::open(&path).is_err());
}

#[test]
fn failed_persistence_preserves_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("directory-in-the-way");
    let mut store = PairStore::<PairedClient>::open(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let key = DeviceKey::from_seed(&[9; 32]);
    assert!(
        store
            .insert(PairedClient {
                public_key: key.public_key(),
                name: "Phone".into(),
                profile: Profile::Read
            })
            .is_err()
    );
    assert_eq!(store.devices().count(), 0);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
