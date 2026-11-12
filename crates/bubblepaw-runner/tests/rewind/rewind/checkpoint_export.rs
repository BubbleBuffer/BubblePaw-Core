use super::*;

/// Both JSONL exports carry the session's checkpoint records with their preimages.
#[tokio::test]
async fn exports_carry_checkpoint_records_and_preimages() {
    use base64::Engine;
    let f = Fixture::new();
    fs::write(f.root.path().join("a"), "one\n").unwrap();
    f.edit("a", "one", "two").await;
    let (id, metadata) = f.records(&f.session).remove(0);
    let preimage = f
        .store
        .checkpoint_preimage(&f.session, &id)
        .unwrap()
        .unwrap();
    let expected_metadata: Value = serde_json::from_str(&metadata).unwrap();

    let mut single = Vec::new();
    f.store.export_jsonl(&f.session, &mut single).unwrap();
    let mut data_dir = Vec::new();
    Store::export_data_dir_jsonl(f._data.path(), &f.session, &mut data_dir).unwrap();
    for bytes in [single, data_dir] {
        let checkpoints: Vec<Value> = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .filter_map(|line| {
                serde_json::from_str::<Value>(line)
                    .unwrap()
                    .get("checkpoints")
                    .cloned()
            })
            .collect();
        assert_eq!(
            checkpoints.len(),
            1,
            "one message record carries the checkpoints"
        );
        let entries = checkpoints[0].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry["id"], json!(id));
        assert_eq!(entry["boundary_seq"], expected_metadata["boundary_seq"]);
        assert_eq!(entry["metadata"], expected_metadata);
        assert_eq!(
            entry["preimage_hash"],
            json!(bubblepaw_core::hex_lower(
                <sha2::Sha256 as sha2::Digest>::digest(b"one\n")
            ))
        );
        assert_eq!(entry["encoding"], json!("base64"));
        let data = base64::engine::general_purpose::STANDARD
            .decode(entry["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(data, preimage);
        assert_eq!(data, b"one\n");
    }
}
