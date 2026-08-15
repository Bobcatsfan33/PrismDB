//! Handing the CLI a key service (S14, [D-095](../../../docs/DECISIONS.md),
//! [encryption contract §11](../../../docs/ENCRYPTION-CONTRACT.md)).
//!
//! Without this, every `prism` subcommand is a keyless process — which means `prism hydrate` could
//! never restore an encrypted store and `prism recover` could never replay a sealed admission log.
//! An encryption feature that only the test suite can operate is not a feature a deployment has.
//!
//! **This is the staging backend, and the environment variable says so.** Production custody is AWS
//! KMS ([§1](../../../docs/ENCRYPTION-CONTRACT.md)); a keystore file is how a *staging* process is
//! given the software keystore's wrapping keys, and every receipt it produces names
//! `software-keystore` so nobody can read a staging run as proof of custody. The key ceremony
//! itself stays external, and nothing here claims otherwise.
//!
//! What it will not do: guess. A missing variable means no key service, not a default key. A
//! keystore other principals can read is refused rather than used. And no path through this module
//! ever renders key material — not in an error, not in a log, not in the JSON a command emits.

use prism_engine::aws_kms::{
    AwsCredentialsProvider, AwsKmsConfig, AwsKmsProvider, SharedCredentialsFile,
    StaticAwsCredentials,
};
use prism_engine::keys::{KeyProvider, SoftwareKeystore};
use prism_engine::storage::sigv4::Credentials;
use prism_types::error::{PrismError, Result};
use std::sync::Arc;
use zeroize::Zeroizing;

/// Where a staging process is told to find its wrapping keys.
///
/// Named for what it is. An operator who sets `PRISM_STAGING_KEYSTORE_FILE` in production has been
/// told by the variable itself that they are not using the production key custody path.
pub const KEYSTORE_ENV: &str = "PRISM_STAGING_KEYSTORE_FILE";
pub const KMS_KEY_ENV: &str = "PRISM_KMS_KEY_ID";
pub const KMS_REGION_ENV: &str = "PRISM_KMS_REGION";
pub const KMS_CREDENTIALS_FILE_ENV: &str = "PRISM_KMS_CREDENTIALS_FILE";
pub const KMS_PROFILE_ENV: &str = "PRISM_KMS_PROFILE";
pub const KMS_DECRYPT_KEYS_ENV: &str = "PRISM_KMS_DECRYPT_KEY_IDS";

/// The on-disk form: which key is active, and the wrapping keys this process holds.
///
/// Every key present is authorized to **unwrap**; only `active` is used to **wrap**. That is
/// exactly the expand/activate/retire state the rotation contract describes — a key dropped from
/// this file is a key this process can no longer open anything with.
#[derive(serde::Deserialize)]
struct KeystoreFile {
    active: String,
    /// Key id → 32 bytes of wrapping key, lowercase hex.
    keys: std::collections::BTreeMap<String, String>,
}

/// 32 bytes, as hex. Errors name the **key id**, never the value — a parse error that echoed the
/// material would put a wrapping key in a shell history and a CI log at once.
fn decode_key(key_id: &str, hex: &str) -> Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return Err(PrismError::Invalid(format!(
            "wrapping key `{key_id}` must be 64 hex characters (32 bytes); it is {} characters",
            hex.len()
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| {
            PrismError::Invalid(format!(
                "wrapping key `{key_id}` is not valid hex at byte {i}"
            ))
        })?;
    }
    Ok(out)
}

/// Refuse a keystore any other principal on this host can read.
///
/// A wrapping key readable by every account on the box is not custody, and discovering that after a
/// restore is discovering it too late. Unix only, because that is where the mode bits mean this.
#[cfg(unix)]
fn assert_not_world_readable(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(PrismError::Policy(format!(
            "refusing to read the keystore at `{}`: mode {mode:o} lets group or other read it. \
             A wrapping key every account on this host can read is not key custody; \
             `chmod 600` it.",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn assert_not_world_readable(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

/// The key service this process was configured with, or `None` for a plaintext store.
///
/// Explicit by construction: no ambient default, and nothing inferred from the bytes already in the
/// data directory. A store is encrypted because it was configured to be
/// ([§10](../../../docs/ENCRYPTION-CONTRACT.md)).
pub fn from_env() -> Result<Option<Arc<dyn KeyProvider>>> {
    let staging = std::env::var_os(KEYSTORE_ENV).filter(|value| !value.is_empty());
    let kms_key = std::env::var(KMS_KEY_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    if staging.is_some() && kms_key.is_some() {
        return Err(PrismError::Invalid(format!(
            "{KEYSTORE_ENV} and {KMS_KEY_ENV} select different key backends; configure exactly one"
        )));
    }
    if let Some(key_arn) = kms_key {
        return Ok(Some(kms_from_env(key_arn)?));
    }
    let Some(path) = staging else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(path);
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    assert_not_world_readable(&path)?;

    // Zeroized on drop: the file's bytes are key material until this function returns.
    let raw = Zeroizing::new(std::fs::read(&path).map_err(|e| {
        PrismError::Invalid(format!(
            "cannot read the keystore named by {KEYSTORE_ENV} at `{}`: {e}",
            path.display()
        ))
    })?);
    let parsed: KeystoreFile = serde_json::from_slice(&raw).map_err(|e| {
        // `e` describes the *shape* -- a line and a missing field -- and never a value, because
        // serde_json's errors do not echo the input.
        PrismError::Invalid(format!(
            "the keystore at `{}` is not readable as a keystore: {e}",
            path.display()
        ))
    })?;

    let active = parsed.active.clone();
    let active_bytes = Zeroizing::new(decode_key(
        &active,
        parsed.keys.get(&active).ok_or_else(|| {
            PrismError::Invalid(format!(
                "the keystore at `{}` names `{active}` as active but does not hold it",
                path.display()
            ))
        })?,
    )?);
    let store = SoftwareKeystore::new(active.clone(), *active_bytes);

    // Everything else is expanded in: authorized to unwrap, never used to wrap. This is what lets a
    // backup taken before a rotation still restore.
    for (key_id, hex) in &parsed.keys {
        if key_id == &active {
            continue;
        }
        let bytes = Zeroizing::new(decode_key(key_id, hex)?);
        store.expand(key_id.clone(), *bytes)?;
    }
    Ok(Some(Arc::new(store)))
}

fn kms_from_env(key_arn: String) -> Result<Arc<dyn KeyProvider>> {
    let region = std::env::var(KMS_REGION_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            PrismError::Invalid(format!(
                "{KMS_KEY_ENV} requires an explicit non-empty {KMS_REGION_ENV}"
            ))
        })?;
    let credentials: Arc<dyn AwsCredentialsProvider> = if let Some(path) =
        std::env::var_os(KMS_CREDENTIALS_FILE_ENV).filter(|value| !value.is_empty())
    {
        let profile = std::env::var(KMS_PROFILE_ENV).unwrap_or_else(|_| "default".into());
        Arc::new(SharedCredentialsFile::new(path.into(), profile)?)
    } else {
        let required = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    PrismError::Invalid(format!(
                        "AWS KMS requires {name}, or a refreshable {KMS_CREDENTIALS_FILE_ENV}"
                    ))
                })
        };
        Arc::new(StaticAwsCredentials(Credentials {
            access_key: required("AWS_ACCESS_KEY_ID")?,
            secret_key: required("AWS_SECRET_ACCESS_KEY")?,
            session_token: std::env::var("AWS_SESSION_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        }))
    };
    let decrypt_keys = std::env::var(KMS_DECRYPT_KEYS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let config = AwsKmsConfig::production(region, key_arn, credentials)?
        .with_decrypt_key_arns(decrypt_keys)?;
    Ok(Arc::new(AwsKmsProvider::new(config)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &std::path::Path, body: &str, mode: u32) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("keys.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        path
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "prism-keystore-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    const BODY: &str = r#"{"active":"key-v2","keys":{
        "key-v1":"1111111111111111111111111111111111111111111111111111111111111111",
        "key-v2":"2222222222222222222222222222222222222222222222222222222222222222"}}"#;

    #[test]
    fn a_keystore_other_accounts_can_read_is_refused_by_name() {
        let dir = tmp("perms");
        let path = write(&dir, BODY, 0o644);
        let err = assert_not_world_readable(&path).unwrap_err().to_string();
        assert!(err.contains("not key custody"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_private_keystore_loads_with_every_key_authorized_to_unwrap() {
        let dir = tmp("load");
        let path = write(&dir, BODY, 0o600);
        assert_not_world_readable(&path).unwrap();

        let parsed: KeystoreFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let store = SoftwareKeystore::new(
            parsed.active.clone(),
            decode_key("key-v2", &parsed.keys["key-v2"]).unwrap(),
        );
        store
            .expand(
                "key-v1",
                decode_key("key-v1", &parsed.keys["key-v1"]).unwrap(),
            )
            .unwrap();

        assert_eq!(store.active_key_id().unwrap(), "key-v2");
        // The non-active key still opens what it sealed -- the retired-but-authorized state.
        let dek = prism_part::crypto::generate_dek().unwrap();
        let wrapped = prism_part::crypto::wrap_dek(&[0x11; 32], "key-v1", &dek).unwrap();
        assert_eq!(&*store.unwrap("key-v1", &wrapped).unwrap(), &*dek);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_key_that_is_not_32_bytes_is_refused_without_echoing_it() {
        let err = decode_key("key-v1", "abcd").unwrap_err().to_string();
        assert!(err.contains("64 hex characters"), "{err}");
        assert!(
            !err.contains("abcd"),
            "an error must never echo key material: {err}"
        );
    }

    #[test]
    fn a_non_hex_key_is_refused_without_echoing_it() {
        let bad = "z".repeat(64);
        let err = decode_key("key-v1", &bad).unwrap_err().to_string();
        assert!(err.contains("not valid hex"), "{err}");
        assert!(!err.contains(&bad), "an error must never echo key material");
    }

    #[test]
    fn an_unset_variable_means_no_key_service_rather_than_a_default_key() {
        std::env::remove_var(KEYSTORE_ENV);
        assert!(from_env().unwrap().is_none());
    }
}
