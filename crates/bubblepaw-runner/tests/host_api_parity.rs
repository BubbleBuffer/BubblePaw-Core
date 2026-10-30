//! Transport parity for the host API table. Every `host_api::METHODS` entry has
//! a fixture, and each fixture runs through the real `stdio` process, through
//! `POST /rpc` of the real `web` process, and through the host-API client,
//! attached to a `stdio` host's endpoint and embedded on the host context. All
//! share one data directory: each runs every step and shuts down, then the next
//! runs the same steps against its own paired sessions. Normalized replies must match step
//! for step, so an adapter that drops, renames or rewrites a field fails here.
#![cfg(feature = "web")]

#[path = "support/web_host.rs"]
mod web_host;

#[path = "host_api_parity/fixtures.rs"]
mod fixtures;
#[path = "host_api_parity/fixtures_runs.rs"]
mod fixtures_runs;
#[path = "host_api_parity/fixtures_services.rs"]
mod fixtures_services;
#[path = "host_api_parity/fixtures_sessions.rs"]
mod fixtures_sessions;
#[path = "host_api_parity/framing.rs"]
mod framing;
#[path = "host_api_parity/gates.rs"]
mod gates;
#[path = "host_api_parity/normalize.rs"]
mod normalize;
#[path = "host_api_parity/peer.rs"]
mod peer;
#[path = "host_api_parity/run.rs"]
mod run;
#[path = "host_api_parity/same_store.rs"]
mod same_store;
#[path = "host_api_parity/same_store_scope.rs"]
mod same_store_scope;

use bubblepaw_runner::host_api::METHODS;
use std::collections::BTreeSet;

/// A new table method either arrives with a fixture or fails here by name.
#[test]
fn every_table_method_has_exactly_one_fixture_that_exercises_it() {
    let fixtures = fixtures::all();
    let table: BTreeSet<&str> = METHODS.iter().map(|method| method.name).collect();
    let mut covered = BTreeSet::new();
    for fixture in &fixtures {
        assert!(
            covered.insert(fixture.method),
            "more than one parity fixture for {}",
            fixture.method
        );
        assert!(
            fixture
                .steps
                .iter()
                .any(|step| step.method == fixture.method),
            "parity fixture {} never calls its own method",
            fixture.method
        );
    }
    let missing: Vec<&str> = table.difference(&covered).copied().collect();
    assert!(
        missing.is_empty(),
        "host_api table methods without a parity fixture: {missing:?}"
    );
    let stale: Vec<&str> = covered.difference(&table).copied().collect();
    assert!(
        stale.is_empty(),
        "parity fixtures for methods not in the table: {stale:?}"
    );
}

/// Every table method, through every transport, against one store: stdio,
/// `/rpc`, and the host-API client attached to a CLI host's endpoint and
/// embedded on the host context.
#[tokio::test]
async fn every_transport_agrees_on_every_table_method() {
    let root = tempfile::tempdir().unwrap();
    peer::seed(root.path());
    let fixtures = fixtures::all();
    let stdio = run::phase(peer::Transport::Stdio, root.path(), "stdio", &fixtures).await;
    let rpc = run::phase(peer::Transport::Rpc, root.path(), "http", &fixtures).await;
    run::compare(&stdio, &rpc);
    let attached = run::phase(
        peer::Transport::Attached,
        root.path(),
        "attached",
        &fixtures,
    )
    .await;
    run::compare(&stdio, &attached);
    let embedded = run::phase(
        peer::Transport::Embedded,
        root.path(),
        "embedded",
        &fixtures,
    )
    .await;
    run::compare(&stdio, &embedded);
}
