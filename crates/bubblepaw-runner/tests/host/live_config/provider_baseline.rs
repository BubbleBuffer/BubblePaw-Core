//! `providers.save` and `providers.remove` move the live config's baseline, so a
//! turn-start check and a later `providers.reload` see nothing left to apply.
use super::{config_set::setup, *};

async fn assert_baseline_current(
    fixture: &Fixture,
    setup: &bubblepaw_runner::provider_setup::ProviderSetup,
    step: &str,
) {
    let checked = fixture.live.check().await;
    assert!(
        checked.applied && checked.changed.is_empty(),
        "{step}: check {checked:?}"
    );
    let reloaded = setup.reload().await.unwrap();
    assert_eq!(reloaded["applied"], true, "{step}");
    assert_eq!(
        reloaded["diff"]["changed"],
        json!([]),
        "{step}: reload {reloaded}"
    );
}

const EXTRA: &str = "[providers.extra]
kind='scripted'
models=[{id='m'}]
";

#[tokio::test]
async fn provider_baseline_follows_save_and_remove() {
    // (label, starting config, save instead of remove, file holds the provider afterwards)
    for (label, start, save, present) in [
        ("provider_save", "", true, true),
        ("provider_remove", EXTRA, false, false),
    ] {
        let fixture = Fixture::new(&config(start)).await;
        let setup = setup(&fixture);
        if save {
            setup
                .save("extra", json!({"kind":"scripted","models":[{"id":"m"}]}))
                .await
                .unwrap();
        } else {
            setup.remove("extra").await.unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&fixture.file)
                .unwrap()
                .contains("[providers.extra]"),
            present,
            "{label}: provider presence in the config file"
        );
        assert_baseline_current(&fixture, &setup, label).await;
        fixture.shutdown().await;
    }
}
