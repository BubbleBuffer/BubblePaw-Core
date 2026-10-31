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
async fn provider_save_leaves_no_pending_change_for_check_or_reload() {
    let fixture = Fixture::new(&config("")).await;
    let setup = setup(&fixture);
    setup
        .save("extra", json!({"kind":"scripted","models":[{"id":"m"}]}))
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(&fixture.file)
            .unwrap()
            .contains("[providers.extra]")
    );
    assert_baseline_current(&fixture, &setup, "after save").await;
    fixture.shutdown().await;
}

#[tokio::test]
async fn provider_remove_leaves_no_pending_change_for_check_or_reload() {
    let fixture = Fixture::new(&config(EXTRA)).await;
    let setup = setup(&fixture);
    setup.remove("extra").await.unwrap();
    assert!(
        !std::fs::read_to_string(&fixture.file)
            .unwrap()
            .contains("[providers.extra]")
    );
    assert_baseline_current(&fixture, &setup, "after remove").await;
    fixture.shutdown().await;
}
