//! One-time import of legacy per-workspace stores into the central store.
//! Core imports chats and quota history; the host merges its delegation ledger.
use crate::delegate::ledger::{Ledger, LedgerMerge};
use anyhow::{Context, Result};
use bubblepaw_core::{LEGACY_IMPORT_BATCH, LegacyImportStatus, LegacyImportSummary, Store};
use std::path::PathBuf;

/// The platform data directory under `HOME` when neither `LOCALAPPDATA` nor `XDG_DATA_HOME` is set.
const HOME_DATA_DIR: &str = if cfg!(target_os = "macos") {
    "Library/Application Support"
} else {
    ".local/share"
};

/// The central store's folder name under the local data home. Until the
/// rename it was `BubblePaw-base`, and that store is still imported.
const CENTRAL_DIR: &str = "BubblePaw-Core";
const PREVIOUS_CENTRAL_DIR: &str = "BubblePaw-base";

/// One central store per user and machine: `<local data home>/BubblePaw-Core/store`.
/// Legacy per-workspace stores are its 64-hex-named siblings.
pub fn default_data_dir() -> Result<PathBuf> {
    Ok(central_under(&local_data_home()?))
}

/// The previous default store, `<local data home>/BubblePaw-base/store`, as a
/// legacy source of the central store (with the stores beside it). Only the
/// default location uses it; an explicit `data_dir` never does.
pub fn previous_default_stores() -> Result<Vec<PathBuf>> {
    Ok(vec![previous_central_under(&local_data_home()?)])
}

fn central_under(local: &std::path::Path) -> PathBuf {
    local.join(CENTRAL_DIR).join("store")
}

fn previous_central_under(local: &std::path::Path) -> PathBuf {
    local.join(PREVIOUS_CENTRAL_DIR).join("store")
}

fn local_data_home() -> Result<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(HOME_DATA_DIR)))
        .context("choose --data-dir when no local data directory is available")
}

/// What one host start imported, for notices and measurements.
#[derive(Debug, Default)]
pub struct CentralImport {
    pub summary: LegacyImportSummary,
    pub ledger: LedgerMerge,
}

/// Whether the central store still has a legacy source to import (the
/// previous default store or a per-workspace store beside either).
pub fn import_pending(store: &Store) -> Result<bool> {
    store.legacy_import_pending_with(&previous_default_stores()?)
}

/// Import the default location's legacy sources; see `import_legacy_stores_with`.
pub fn import_legacy_stores(store: &Store) -> Result<CentralImport> {
    import_legacy_stores_with(store, &previous_default_stores()?)
}

/// Import every legacy source not yet recorded in the central catalog: the
/// `previous` default stores, then the per-workspace stores beside them.
/// Sources are only read; locked or failing sources are retried on a later start.
pub fn import_legacy_stores_with(store: &Store, previous: &[PathBuf]) -> Result<CentralImport> {
    let data_dir = store
        .data_dir()
        .ok_or_else(|| anyhow::anyhow!("legacy import requires a directory store"))?
        .to_owned();
    let mut ledger: Option<Ledger> = None;
    let mut merged = LedgerMerge::default();
    let summary = store.import_legacy_stores_with(previous, |snapshot| {
        let copy = snapshot.dir.join("delegations.db");
        if !copy.is_file() {
            return Ok(());
        }
        if ledger.is_none() {
            ledger = Some(Ledger::open(&data_dir.join("delegations.db"))?);
        }
        let merge = ledger
            .as_mut()
            .unwrap()
            .merge_copy(&copy, LEGACY_IMPORT_BATCH)?;
        merged.attempts += merge.attempts;
        merged.calls += merge.calls;
        merged.transactions += merge.transactions;
        Ok(())
    })?;
    Ok(CentralImport {
        summary,
        ledger: merged,
    })
}

/// Human-readable notices for stderr. Already-imported sources stay silent.
pub fn notices(import: &CentralImport) -> Vec<String> {
    import
        .summary
        .sources
        .iter()
        .flat_map(|report| {
            let source = report.source.display();
            let mut lines = Vec::new();
            match &report.status {
                LegacyImportStatus::AlreadyImported => {}
                LegacyImportStatus::Imported => {
                    lines.push(format!(
                        "Imported legacy store {source}: {} chats, {} quota rows; the source was left unchanged",
                        report.chats, report.quota_rows
                    ));
                    if !report.duplicates.is_empty() {
                        lines.push(format!(
                            "Legacy store {source}: {} chats were already in the central store with the same content and were not copied again",
                            report.duplicates.len()
                        ));
                    }
                    for (old, new) in &report.renamed {
                        lines.push(format!(
                            "Legacy store {source}: chat {old} differs from the central chat with that id; imported it as {new}"
                        ));
                    }
                }
                LegacyImportStatus::Locked => lines.push(format!(
                    "Legacy store {source} is in use by another process; it will be imported on a later start"
                )),
                LegacyImportStatus::Skipped(reason) => lines.push(format!(
                    "Legacy store {source} was not imported ({reason}); it will be retried on a later start"
                )),
                LegacyImportStatus::Unsupported(reason) => lines.push(format!(
                    "Legacy store {source} was not imported ({reason}); this notice is not repeated"
                )),
            }
            lines
        })
        .collect()
}
