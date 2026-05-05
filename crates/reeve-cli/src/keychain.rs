//! Platform keychain factory.
//!
//! Centralises the `#[cfg(target_os)]` keychain construction so that
//! `cmd_enroll`, `cmd_envelope_sign`, and `adapter::dispatch` all use the
//! same code path instead of duplicating the platform branch.

// ── OperatorKeyStore factory ───────────────────────────────────────────────────

/// Construct the platform keychain backend that implements [`OperatorKeyStore`].
///
/// On macOS this is [`MacOsKeyStore`]; on other Unix systems it opens a
/// D-Bus connection to the Secret Service daemon. Errors on Linux if the
/// daemon is unavailable.
///
/// The macOS variant returns `Ok` unconditionally; `Result` is used on both
/// platforms so callers can apply `?` uniformly.
#[cfg(target_os = "macos")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "Result interface is kept uniform across platform variants so callers use `?` on both"
)]
pub(crate) fn open_platform_keystore(
) -> Result<reeve_runtime::keychain::macos::MacOsKeyStore, Box<dyn std::error::Error>> {
    Ok(reeve_runtime::keychain::macos::MacOsKeyStore::new())
}

/// Construct the platform keychain backend that implements [`OperatorKeyStore`].
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn open_platform_keystore(
) -> Result<reeve_runtime::keychain::linux::SecretServiceKeyStore, Box<dyn std::error::Error>> {
    Ok(reeve_runtime::keychain::linux::SecretServiceKeyStore::connect()?)
}

// ── OperatorSecretStore factory ────────────────────────────────────────────────

/// Construct the platform keychain backend that implements [`OperatorSecretStore`].
///
/// The macOS and Linux backends implement both [`OperatorKeyStore`] and
/// [`OperatorSecretStore`]; these are separate factory functions because the
/// return types differ and Rust does not have existential `impl Trait` in
/// return position with platform branching.
///
/// The macOS variant returns `Ok` unconditionally; `Result` is used on both
/// platforms so callers can apply `?` uniformly.
#[cfg(target_os = "macos")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "Result interface is kept uniform across platform variants so callers use `?` on both"
)]
pub(crate) fn open_platform_secretstore(
) -> Result<reeve_runtime::keychain::macos::MacOsKeyStore, Box<dyn std::error::Error>> {
    Ok(reeve_runtime::keychain::macos::MacOsKeyStore::new())
}

/// Construct the platform keychain backend that implements [`OperatorSecretStore`].
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) fn open_platform_secretstore(
) -> Result<reeve_runtime::keychain::linux::SecretServiceKeyStore, Box<dyn std::error::Error>> {
    Ok(reeve_runtime::keychain::linux::SecretServiceKeyStore::connect()?)
}
