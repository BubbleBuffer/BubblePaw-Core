use super::*;

pub(super) fn parse_kdf(
    mut entries: Vec<(String, Json)>,
) -> Result<[u8; SALT_BYTES], DeviceKeyError> {
    if take_str(&mut entries, "alg", "kdf.alg")?.as_str() != KDF_ALG {
        return Err(DeviceKeyError::UnsupportedKdf);
    }
    let memory = take_int(&mut entries, "m", "kdf.m")?;
    let passes = take_int(&mut entries, "t", "kdf.t")?;
    let lanes = take_int(&mut entries, "p", "kdf.p")?;
    if memory != i64::from(KDF_MEMORY_KIB)
        || passes != i64::from(KDF_PASSES)
        || lanes != i64::from(KDF_LANES)
    {
        return Err(DeviceKeyError::UnsupportedKdf);
    }
    let salt = take_str(&mut entries, "salt", "kdf.salt")?;
    reject_unknown(&entries, "kdf")?;
    decode_fixed::<SALT_BYTES>(&salt, "kdf.salt")
}

pub(super) fn parse_aead(
    mut entries: Vec<(String, Json)>,
) -> Result<[u8; NONCE_BYTES], DeviceKeyError> {
    if take_str(&mut entries, "alg", "aead.alg")?.as_str() != AEAD_ALG {
        return Err(DeviceKeyError::UnsupportedAead);
    }
    let nonce = take_str(&mut entries, "nonce", "aead.nonce")?;
    reject_unknown(&entries, "aead")?;
    decode_fixed::<NONCE_BYTES>(&nonce, "aead.nonce")
}

pub(super) fn take(
    entries: &mut Vec<(String, Json)>,
    key: &str,
    path: &'static str,
) -> Result<Json, DeviceKeyError> {
    let index = entries
        .iter()
        .position(|(k, _)| k.as_str() == key)
        .ok_or(DeviceKeyError::MissingField(path))?;
    Ok(entries.swap_remove(index).1)
}

pub(super) fn take_str(
    entries: &mut Vec<(String, Json)>,
    key: &str,
    path: &'static str,
) -> Result<Zeroizing<String>, DeviceKeyError> {
    match take(entries, key, path)? {
        Json::Str(value) => Ok(value),
        _ => Err(DeviceKeyError::InvalidField(path)),
    }
}

pub(super) fn take_int(
    entries: &mut Vec<(String, Json)>,
    key: &str,
    path: &'static str,
) -> Result<i64, DeviceKeyError> {
    match take(entries, key, path)? {
        Json::Int(value) => Ok(value),
        _ => Err(DeviceKeyError::InvalidField(path)),
    }
}

pub(super) fn reject_unknown(
    entries: &[(String, Json)],
    scope: &str,
) -> Result<(), DeviceKeyError> {
    match entries.first() {
        None => Ok(()),
        Some((key, _)) if scope.is_empty() => Err(DeviceKeyError::UnknownField(key.clone())),
        Some((key, _)) => Err(DeviceKeyError::UnknownField(format!("{scope}.{key}"))),
    }
}
