//! Attributes a typed quota error to the provider and account that produced it.
use bubblepaw_core::ProviderError;

pub(super) fn normalize_error(
    error: anyhow::Error,
    provider: &str,
    account: &str,
) -> anyhow::Error {
    let Some(mut typed) = ProviderError::of(&error).cloned() else {
        return error;
    };
    if !typed
        .quota
        .iter()
        .any(|reading| reading.provider != provider || reading.account != account)
    {
        return error;
    }
    for reading in &mut typed.quota {
        reading.provider = provider.into();
        reading.account = account.into();
    }
    error.context(typed)
}
