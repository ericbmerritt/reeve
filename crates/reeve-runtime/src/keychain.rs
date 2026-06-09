//! OS keychain integration for operator private keys and labeled secrets per
//! `specs/reeve-walking-skeleton.ladder.md` phase 2 task 3 and phase 5 task 3,
//! and `specs/reeve-transport-security.md` § Session Authentication.
//!
//! Domain-model invariant 5 says operator and external private keys are never
//! stored in the agent filesystem tree. The OS credential store is *not* the
//! agent filesystem — it is the operator's per-user credential vault — and is
//! the only durable home for private key bytes between sessions. This module
//! is the single place those bytes round-trip through.
//!
//! The public surface is two traits. All three backends implement both:
//!
//! - [`OperatorKeyStore`] — round-trips fixed-length ed25519 seed bytes,
//!   keyed by [`IdentityId`].
//! - [`OperatorSecretStore`] — stores and retrieves opaque, string-shaped
//!   secrets (API keys, OAuth tokens, etc.), keyed by a human-readable label.
//!
//! Three implementations ship in this crate:
//!
//! - `macos::MacOsKeyStore` — backed by Apple's Security.framework generic
//!   password API. Active on `target_os = "macos"`.
//! - `linux::SecretServiceKeyStore` — backed by the freedesktop Secret
//!   Service via `secret-service`'s blocking interface. Active on Unix targets
//!   other than macOS, and requires a running `gnome-keyring-daemon` or
//!   `kwallet` providing the Secret Service API on the session bus.
//! - [`memory::MemoryKeyStore`] — process-local in-memory map. Used by
//!   trait-contract tests and as a dependency-injection seam for higher-layer
//!   tests; not suitable for production because seeds are lost on process
//!   exit.
//!
//! The traits are the public types so callers in later phases can take
//! `dyn OperatorKeyStore` or `dyn OperatorSecretStore` and let tests inject
//! the in-memory backend without `cfg(test)` gymnastics.
//!
//! ## Service name
//!
//! All entries are stored under the service `"reeve"` ([`KEYCHAIN_SERVICE`]).
//! On macOS this is the generic-password `service` field; on Secret Service
//! it is the value of the `"service"` attribute. Per-identity entries are
//! disambiguated by the hyphenated `IdentityId` UUID.
//!
//! ## Zeroize discipline
//!
//! Seed bytes flow through this module wrapped in [`Zeroizing`] from the
//! `zeroize` crate. The platform crates may copy the bytes into their own
//! buffers internally — that is outside this module's control — but every
//! buffer this module owns is wiped on drop. Seeds never appear in
//! [`std::fmt::Display`], [`std::fmt::Debug`], or [`std::error::Error`]
//! output.
//!
//! ## Single-operator policy
//!
//! The trait shape supports any number of identities. The single-operator-
//! per-machine invariant is enforced one layer up by the CLI in task 4, so
//! this module does not encode a uniqueness gate.

use reeve_types::IdentityId;
use zeroize::Zeroizing;

pub mod linux;
pub mod macos;
pub mod memory;

/// Well-known keychain entry labels used by Reeve subsystems.
///
/// Lives in `reeve-runtime` (not `reeve-adapter`) because the keychain
/// abstraction lives here; the adapter crate stays free of OS-keyring
/// dependencies. The CLI (Task 18) reads via `keychain.retrieve_secret(
/// labels::ANTHROPIC_API_KEY)` and passes the `SecretString` to the
/// adapter's constructor.
pub mod labels {
    /// Label under which the Anthropic API key is stored.
    ///
    /// The schema `reeve-{provider}-api-key` makes labels operator-readable
    /// in macOS Keychain Access UI and gnome-keyring listings while
    /// remaining unambiguous about provenance. Future provider keys
    /// follow the same shape (e.g., `reeve-openai-api-key`,
    /// `reeve-openrouter-api-key`).
    ///
    /// See `specs/reeve-walking-skeleton.ladder.md` Phase 5.
    pub const ANTHROPIC_API_KEY: &str = "reeve-anthropic-api-key";

    /// Label under which the `OpenRouter` API key is stored.
    ///
    /// Used by adapters that route through `openrouter.ai` (e.g.
    /// `DeepSeekR1OpenRouter`). Follows the same `reeve-{provider}-api-key`
    /// schema as [`ANTHROPIC_API_KEY`].
    pub const OPENROUTER_API_KEY: &str = "reeve-openrouter-api-key";
}

/// Service name used for every reeve keychain entry. The macOS generic-
/// password `service` field and the Secret Service `service` attribute both
/// take this value.
pub const KEYCHAIN_SERVICE: &str = "reeve";

/// Length of an ed25519 signing seed. Re-exported here so callers do not
/// need to depend on `ed25519-dalek` directly to size buffers correctly.
pub const SEED_LEN: usize = 32;

/// Round-trip private-key seeds against the operator's OS credential store.
///
/// Implementations are thread-safe (`Send + Sync`) — callers may share a
/// single store across threads.
///
/// - [`store`](Self::store): Replaces any existing entry for the same
///   identity (idempotent overwrite — key rotation happens above this layer).
/// - [`retrieve`](Self::retrieve): Returns [`KeychainError::NotFound`] if no
///   entry exists for the given identity.
/// - [`delete`](Self::delete): Returns [`KeychainError::NotFound`] if no
///   entry existed. Higher layers that want idempotent removal can swallow
///   `NotFound` themselves.
///
/// Implementations must hold the seed bytes in [`Zeroizing`] containers
/// throughout the call so any owned buffer is wiped on drop. Implementations
/// must not log, display, or debug-format the seed.
pub trait OperatorKeyStore: Send + Sync {
    /// Store `seed` under the entry for `identity_id`. Replaces any existing
    /// entry for the same identity.
    fn store(
        &self,
        identity_id: IdentityId,
        seed: &Zeroizing<[u8; SEED_LEN]>,
    ) -> Result<(), KeychainError>;

    /// Retrieve the seed for `identity_id`. Returns [`KeychainError::NotFound`]
    /// if no entry exists for this identity.
    fn retrieve(&self, identity_id: IdentityId)
        -> Result<Zeroizing<[u8; SEED_LEN]>, KeychainError>;

    /// Delete the entry for `identity_id`. Returns [`KeychainError::NotFound`]
    /// if no entry existed.
    fn delete(&self, identity_id: IdentityId) -> Result<(), KeychainError>;
}

/// Stores and retrieves opaque, string-shaped secrets in the OS keychain
/// keyed by a label (not an [`IdentityId`]).
///
/// Distinct from [`OperatorKeyStore`] which handles fixed-length seed
/// bytes for ed25519 identities. Use this trait for credentials whose
/// shape is provider-defined (API keys, OAuth tokens, etc.).
///
/// # Security
///
/// Implementations MUST hold returned secrets in [`secrecy::SecretString`]
/// (which zeroizes the inner allocation on drop) throughout the call.
/// Implementations MUST NOT log, display, or debug-format the secret
/// value or the label-bound metadata that could correlate users to
/// services.
///
/// # Label discipline
///
/// `label` SHOULD be a static string constant from [`labels`], not
/// runtime-derived input. Labels appear in:
/// - macOS Keychain Access UI (account field)
/// - gnome-keyring / `KWallet` UI (item label attribute)
/// - error messages (`SecretNotFound`, `MacOsKeychainForLabel`,
///   `SecretServiceForLabel`) which propagate via `Display` and may
///   reach structured logs
///
/// On Linux (Secret Service backend), `label` is also embedded verbatim
/// into the human-readable item label as `"reeve secret for {label}"`.
/// This is permanently inscribed in the desktop keyring UI
/// (gnome-keyring, `KWallet`) until the entry is deleted.
/// Caller-supplied dynamic labels will leave a permanent UI trace.
///
/// Operator-supplied or tool-derived labels expose internal naming
/// to anyone with same-UID access; pin them at compile time.
pub trait OperatorSecretStore: Send + Sync {
    /// Store `secret` under `label`. Replaces any existing entry under the
    /// same label.
    fn store_secret(&self, label: &str, secret: secrecy::SecretString)
        -> Result<(), KeychainError>;

    /// Retrieve the secret stored under `label`.
    ///
    /// Returns [`KeychainError::SecretNotFound`] if no entry exists for the
    /// label. Returns [`KeychainError::InvalidSecretEncoding`] if an entry
    /// exists but its bytes are not valid UTF-8 (indicates corruption or an
    /// entry written by a foreign tool).
    fn retrieve_secret(&self, label: &str) -> Result<secrecy::SecretString, KeychainError>;

    /// Delete the entry under `label`. Returns
    /// [`KeychainError::SecretNotFound`] if no entry existed.
    fn delete_secret(&self, label: &str) -> Result<(), KeychainError>;
}

/// Errors surfaced by the operator key store.
///
/// Variants are typed and platform-gated where the underlying error type is
/// platform-specific. `KeychainError` is not [`Clone`] or [`PartialEq`]:
/// the wrapped backend errors are neither.
///
/// The seed bytes never appear in any variant — only the [`IdentityId`]
/// they were keyed under.
#[non_exhaustive]
pub enum KeychainError {
    /// No entry for the given identity. Returned by both
    /// [`OperatorKeyStore::retrieve`] and [`OperatorKeyStore::delete`].
    NotFound { identity_id: IdentityId },

    /// No keychain entry for the given label. Returned by both
    /// [`OperatorSecretStore::retrieve_secret`] and
    /// [`OperatorSecretStore::delete_secret`].
    ///
    /// The `label` field is non-sensitive metadata — it is a service-name
    /// style identifier (e.g., `"reeve-anthropic-api-key"`), not a user
    /// account binding, and safe to include in error messages.
    SecretNotFound { label: String },

    /// Likely indicates an entry written by a foreign tool, an older Reeve
    /// schema, or out-of-band corruption. Distinguished from
    /// [`SecretNotFound`](Self::SecretNotFound) so callers do not
    /// misinterpret corruption as "not yet configured."
    InvalidSecretEncoding { label: String },

    /// A retrieved entry exists but its byte length is not [`SEED_LEN`].
    /// Likely indicates an entry written by a foreign tool, an older Reeve
    /// schema, or out-of-band corruption.
    InvalidSeedLength { identity_id: IdentityId, len: usize },

    /// More than one keychain entry matched `(service, identity_id)`. The
    /// Secret Service `replace=true` flag is not guaranteed to be atomic
    /// across all compliant implementations; this variant guards against
    /// orphaned duplicates that could arise under a non-conformant daemon.
    /// Manual cleanup of the keyring is required before the identity can be
    /// used again.
    ///
    /// Active on Unix targets other than macOS only.
    #[cfg(all(unix, not(target_os = "macos")))]
    DuplicateEntry {
        identity_id: IdentityId,
        count: usize,
    },

    /// A macOS Security.framework call returned a non-`errSecItemNotFound`
    /// error. Active on `target_os = "macos"` only.
    #[cfg(target_os = "macos")]
    MacOsKeychain {
        identity_id: IdentityId,
        source: security_framework::base::Error,
    },

    /// A macOS Security.framework call for a label-keyed secret returned a
    /// non-`errSecItemNotFound` error. Active on `target_os = "macos"` only.
    ///
    /// Parallel to [`MacOsKeychain`](Self::MacOsKeychain) for label-keyed
    /// operations where no [`IdentityId`] is available.
    #[cfg(target_os = "macos")]
    MacOsKeychainForLabel {
        label: String,
        source: security_framework::base::Error,
    },

    /// A Secret Service call failed. Active on Unix targets other than
    /// macOS only.
    #[cfg(all(unix, not(target_os = "macos")))]
    SecretService {
        identity_id: IdentityId,
        source: secret_service::Error,
    },

    /// No Secret Service provider is reachable on the session bus. Operator
    /// likely needs to start `gnome-keyring-daemon` or equivalent. Active on
    /// Unix targets other than macOS only.
    #[cfg(all(unix, not(target_os = "macos")))]
    SecretServiceUnavailable { source: secret_service::Error },

    /// A Secret Service call failed for a label-keyed secret. Active on Unix
    /// targets other than macOS only.
    ///
    /// Parallel to [`SecretService`](Self::SecretService) for label-keyed
    /// operations where no [`IdentityId`] is available.
    #[cfg(all(unix, not(target_os = "macos")))]
    SecretServiceForLabel {
        label: String,
        source: secret_service::Error,
    },
}

/// Manual `Debug` impl. Avoids leaking D-Bus topology (socket addresses,
/// object paths, interface names) into debug output by not delegating to the
/// upstream source's `Debug`.
impl std::fmt::Debug for KeychainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { identity_id } => f
                .debug_struct("NotFound")
                .field("identity_id", identity_id)
                .finish(),
            Self::SecretNotFound { label } => f
                .debug_struct("SecretNotFound")
                .field("label", label)
                .finish(),
            Self::InvalidSecretEncoding { label } => f
                .debug_struct("InvalidSecretEncoding")
                .field("label", label)
                .finish(),
            Self::InvalidSeedLength { identity_id, len } => f
                .debug_struct("InvalidSeedLength")
                .field("identity_id", identity_id)
                .field("len", len)
                .finish(),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::DuplicateEntry { identity_id, count } => f
                .debug_struct("DuplicateEntry")
                .field("identity_id", identity_id)
                .field("count", count)
                .finish(),
            #[cfg(target_os = "macos")]
            Self::MacOsKeychain { identity_id, .. } => f
                .debug_struct("MacOsKeychain")
                .field("identity_id", identity_id)
                .finish_non_exhaustive(),
            #[cfg(target_os = "macos")]
            Self::MacOsKeychainForLabel { label, .. } => f
                .debug_struct("MacOsKeychainForLabel")
                .field("label", label)
                .finish_non_exhaustive(),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretService { identity_id, .. } => f
                .debug_struct("SecretService")
                .field("identity_id", identity_id)
                .finish_non_exhaustive(),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretServiceUnavailable { .. } => f
                .debug_struct("SecretServiceUnavailable")
                .finish_non_exhaustive(),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretServiceForLabel { label, .. } => f
                .debug_struct("SecretServiceForLabel")
                .field("label", label)
                .finish_non_exhaustive(),
        }
    }
}

impl std::fmt::Display for KeychainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { identity_id } => {
                write!(f, "no keychain entry for identity {identity_id}")
            }
            Self::SecretNotFound { label } => {
                write!(f, "no keychain entry for label {label}")
            }
            Self::InvalidSecretEncoding { label } => {
                write!(
                    f,
                    "keychain entry for label {label} exists but is not valid UTF-8 \
                     — entry may be corrupt or written by a foreign tool",
                )
            }
            Self::InvalidSeedLength { identity_id, len } => write!(
                f,
                "keychain entry for identity {identity_id} has {len} bytes; expected {SEED_LEN}",
            ),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::DuplicateEntry { identity_id, count } => write!(
                f,
                "multiple keychain entries ({count}) found for identity {identity_id} \
                 — manual cleanup required",
            ),
            #[cfg(target_os = "macos")]
            Self::MacOsKeychain { identity_id, .. } => {
                write!(f, "macOS keychain error for identity {identity_id}")
            }
            #[cfg(target_os = "macos")]
            Self::MacOsKeychainForLabel { label, .. } => {
                write!(f, "macOS keychain error for label {label}")
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretService { identity_id, .. } => {
                write!(f, "secret service error for identity {identity_id}")
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretServiceUnavailable { .. } => {
                write!(f, "secret service unavailable on session bus")
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretServiceForLabel { label, .. } => {
                write!(f, "secret service error for label {label}")
            }
        }
    }
}

impl std::error::Error for KeychainError {
    // SECURITY-NOTE: Error::source returns the raw platform error
    // (security_framework::base::Error or secret_service::Error) for
    // platform-backed variants. Their string forms may include OSStatus
    // descriptions or D-Bus paths. The Display/Debug impls suppress this,
    // but consumers that walk error chains programmatically (anyhow {:#},
    // tracing %, error::Error::source loops) will see the platform detail.
    // This is the canonical Rust error-chain contract; opting out of
    // `Error::source` would break interoperability. Operators relying on
    // log-redaction must apply it at the logging layer.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotFound { .. }
            | Self::SecretNotFound { .. }
            | Self::InvalidSecretEncoding { .. }
            | Self::InvalidSeedLength { .. } => None,
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::DuplicateEntry { .. } => None,
            #[cfg(target_os = "macos")]
            Self::MacOsKeychain { source, .. } => Some(source),
            #[cfg(target_os = "macos")]
            Self::MacOsKeychainForLabel { source, .. } => Some(source),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretService { source, .. } => Some(source),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretServiceUnavailable { source } => Some(source),
            #[cfg(all(unix, not(target_os = "macos")))]
            Self::SecretServiceForLabel { source, .. } => Some(source),
        }
    }
}

/// Platform APIs return `Vec<u8>` with no length guarantee; this is the
/// parse-don't-validate boundary that converts those untyped bytes into the
/// fixed-size seed type the rest of the crate trusts.
///
/// Shared by the macOS and Linux backends; `MemoryKeyStore` does not need it
/// because its input is already `[u8; SEED_LEN]` by construction.
pub(super) fn decode_seed_bytes(
    identity_id: IdentityId,
    bytes: &[u8],
) -> Result<Zeroizing<[u8; SEED_LEN]>, KeychainError> {
    if bytes.len() != SEED_LEN {
        return Err(KeychainError::InvalidSeedLength {
            identity_id,
            len: bytes.len(),
        });
    }
    // zeroize wipes on drop; a plain array would linger in the stack frame.
    let mut out = Zeroizing::new([0_u8; SEED_LEN]);
    out.copy_from_slice(bytes);
    Ok(out)
}

#[cfg(not(any(target_os = "macos", all(unix, not(target_os = "macos")))))]
compile_error!(
    "reeve-runtime keychain: no OperatorKeyStore implementation for this target. \
     Implement OperatorKeyStore for your platform and gate it with #[cfg(...)], \
     or see specs/reeve-walking-skeleton.ladder.md Phase 2 for the platform gate rationale.",
);

#[cfg(test)]
mod test_helpers {
    use std::env;

    use reeve_types::IdentityId;

    use super::{OperatorKeyStore, OperatorSecretStore};

    /// Returns true when the environment opts into live keychain tests.
    /// Gating live tests prevents prompts and orphaned entries on workstations
    /// that have not granted Reeve trust.
    pub(crate) fn live_tests_enabled() -> bool {
        env::var_os("REEVE_KEYCHAIN_LIVE_TESTS").is_some_and(|v| v == "1")
    }

    /// Allocate a unique service name for one test so concurrent runs and
    /// previous-run residue do not collide. The disambiguator is a fresh
    /// `UUIDv7` from the project's own ID machinery.
    pub(crate) fn unique_service(tag: &str) -> String {
        format!("reeve.test.{tag}.{}", IdentityId::new().unwrap())
    }

    /// Swallows delete errors — the test body may have already deleted the entry.
    pub(crate) struct DeleteOnDrop<'a, S>
    where
        S: OperatorKeyStore + ?Sized,
    {
        pub(crate) store: &'a S,
        pub(crate) identity_id: IdentityId,
    }

    impl<S> Drop for DeleteOnDrop<'_, S>
    where
        S: OperatorKeyStore + ?Sized,
    {
        fn drop(&mut self) {
            let _ = self.store.delete(self.identity_id);
        }
    }

    /// Parallel to [`DeleteOnDrop`] for label-keyed secrets.
    pub(crate) struct SecretDeleteOnDrop<'a, S>
    where
        S: OperatorSecretStore + ?Sized,
    {
        pub(crate) store: &'a S,
        pub(crate) label: &'a str,
    }

    impl<S> Drop for SecretDeleteOnDrop<'_, S>
    where
        S: OperatorSecretStore + ?Sized,
    {
        fn drop(&mut self) {
            let _ = self.store.delete_secret(self.label);
        }
    }

    /// Sentinel value used by all live keychain tests. Clearly disposable
    /// (matches no real Anthropic API key shape; verifiable by the
    /// `k_secret_no_log_leak` negative assertion).
    pub(crate) const LIVE_SENTINEL: &str = "live-test-sentinel-not-a-real-key";

    /// Run the full secret-store contract suite against `store`. Every platform
    /// implementation calls this under `REEVE_KEYCHAIN_LIVE_TESTS=1` so the
    /// contracts are verified against the real OS backend. `MemoryKeyStore`'s
    /// isolated `k_secret_*` tests cover memory-specific isolation behaviour
    /// separately.
    pub(crate) fn run_secret_contract_suite(store: &impl OperatorSecretStore) {
        contract_secret_round_trip(store);
        contract_secret_replaces_existing(store);
        contract_secret_retrieve_not_found(store);
        contract_secret_delete_not_found(store);
        contract_secret_store_delete_retrieve(store);
    }

    fn contract_secret_round_trip(store: &impl OperatorSecretStore) {
        use secrecy::ExposeSecret as _;

        let label = format!("contract-rt-{}", IdentityId::new().unwrap());
        let _guard = SecretDeleteOnDrop {
            store,
            label: &label,
        };
        store
            .store_secret(
                &label,
                secrecy::SecretString::from(LIVE_SENTINEL.to_owned()),
            )
            .expect("store_secret failed");
        let retrieved = store
            .retrieve_secret(&label)
            .expect("retrieve_secret failed");
        assert_eq!(
            retrieved.expose_secret(),
            LIVE_SENTINEL,
            "contract_secret_round_trip: value mismatch",
        );
    }

    fn contract_secret_replaces_existing(store: &impl OperatorSecretStore) {
        use secrecy::ExposeSecret as _;

        let label = format!("contract-replace-{}", IdentityId::new().unwrap());
        let _guard = SecretDeleteOnDrop {
            store,
            label: &label,
        };
        store
            .store_secret(&label, secrecy::SecretString::from("first".to_owned()))
            .expect("first store_secret failed");
        store
            .store_secret(&label, secrecy::SecretString::from("second".to_owned()))
            .expect("second store_secret failed");
        let retrieved = store
            .retrieve_secret(&label)
            .expect("retrieve_secret failed");
        assert_eq!(
            retrieved.expose_secret(),
            "second",
            "contract_secret_replaces_existing: expected second value to win",
        );
    }

    fn contract_secret_retrieve_not_found(store: &impl OperatorSecretStore) {
        let label = format!("contract-absent-{}", IdentityId::new().unwrap());
        let err = store
            .retrieve_secret(&label)
            .expect_err("expected SecretNotFound for absent label");
        assert!(
            matches!(err, super::KeychainError::SecretNotFound { .. }),
            "contract_secret_retrieve_not_found: expected SecretNotFound, got {err:?}",
        );
    }

    fn contract_secret_delete_not_found(store: &impl OperatorSecretStore) {
        let label = format!("contract-del-absent-{}", IdentityId::new().unwrap());
        let err = store
            .delete_secret(&label)
            .expect_err("expected SecretNotFound for absent label");
        assert!(
            matches!(err, super::KeychainError::SecretNotFound { .. }),
            "contract_secret_delete_not_found: expected SecretNotFound, got {err:?}",
        );
    }

    fn contract_secret_store_delete_retrieve(store: &impl OperatorSecretStore) {
        let label = format!("contract-sdr-{}", IdentityId::new().unwrap());
        let _guard = SecretDeleteOnDrop {
            store,
            label: &label,
        };
        store
            .store_secret(
                &label,
                secrecy::SecretString::from(LIVE_SENTINEL.to_owned()),
            )
            .expect("store_secret failed");
        store.delete_secret(&label).expect("delete_secret failed");
        let err = store
            .retrieve_secret(&label)
            .expect_err("expected SecretNotFound after delete");
        assert!(
            matches!(err, super::KeychainError::SecretNotFound { .. }),
            "contract_secret_store_delete_retrieve: expected SecretNotFound, got {err:?}",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::keychain::memory::MemoryKeyStore;
    use crate::keychain::test_helpers::DeleteOnDrop;

    fn fresh_seed(byte: u8) -> Zeroizing<[u8; SEED_LEN]> {
        Zeroizing::new([byte; SEED_LEN])
    }

    /// Run the full contract suite against `store`. Every platform
    /// implementation calls this with a fresh store instance so the
    /// contracts are verified against the real backend under live test mode.
    /// `MemoryKeyStore` runs it by default (no env gate needed).
    pub(crate) fn run_contract_suite(store: &impl OperatorKeyStore) {
        contract_store_then_retrieve_round_trips(store);
        contract_store_overwrites_existing_entry_idempotently(store);
        contract_retrieve_missing_returns_not_found(store);
        contract_delete_then_retrieve_returns_not_found(store);
        contract_delete_missing_returns_not_found(store);
        contract_distinct_identities_do_not_collide(store);
        contract_deleting_one_identity_does_not_affect_others(store);
        contract_invalid_seed_length_error_displays_lengths(store);
        contract_not_found_error_displays_identity_id(store);
        contract_retrieve_returns_zeroizing_wrapped_seed(store);
        contract_zero_and_max_byte_seeds_round_trip(store);
    }

    fn contract_store_then_retrieve_round_trips(store: &impl OperatorKeyStore) {
        let id = IdentityId::new().unwrap();
        let seed = fresh_seed(0x11);
        store.store(id, &seed).unwrap();
        let _guard = DeleteOnDrop {
            store,
            identity_id: id,
        };
        let loaded = store.retrieve(id).unwrap();
        assert_eq!(*loaded, *seed);
    }

    fn contract_store_overwrites_existing_entry_idempotently(store: &impl OperatorKeyStore) {
        let id = IdentityId::new().unwrap();
        store.store(id, &fresh_seed(0x11)).unwrap();
        let _guard = DeleteOnDrop {
            store,
            identity_id: id,
        };
        store.store(id, &fresh_seed(0x22)).unwrap();
        let loaded = store.retrieve(id).unwrap();
        assert_eq!(*loaded, [0x22_u8; SEED_LEN]);
    }

    fn contract_retrieve_missing_returns_not_found(store: &impl OperatorKeyStore) {
        let id = IdentityId::new().unwrap();
        let err = store.retrieve(id).unwrap_err();
        let KeychainError::NotFound {
            identity_id: missing,
        } = err
        else {
            panic!("expected NotFound, got {err:?}");
        };
        assert_eq!(missing, id);
    }

    fn contract_delete_then_retrieve_returns_not_found(store: &impl OperatorKeyStore) {
        let id = IdentityId::new().unwrap();
        store.store(id, &fresh_seed(0x11)).unwrap();
        let _guard = DeleteOnDrop {
            store,
            identity_id: id,
        };
        store.delete(id).unwrap();
        let err = store.retrieve(id).unwrap_err();
        assert!(matches!(err, KeychainError::NotFound { .. }));
    }

    fn contract_delete_missing_returns_not_found(store: &impl OperatorKeyStore) {
        let id = IdentityId::new().unwrap();
        let err = store.delete(id).unwrap_err();
        assert!(matches!(err, KeychainError::NotFound { .. }));
    }

    fn contract_distinct_identities_do_not_collide(store: &impl OperatorKeyStore) {
        let id_a = IdentityId::new().unwrap();
        let id_b = IdentityId::new().unwrap();
        store.store(id_a, &fresh_seed(0xAA)).unwrap();
        let _guard_a = DeleteOnDrop {
            store,
            identity_id: id_a,
        };
        store.store(id_b, &fresh_seed(0xBB)).unwrap();
        let _guard_b = DeleteOnDrop {
            store,
            identity_id: id_b,
        };
        let loaded_a = store.retrieve(id_a).unwrap();
        let loaded_b = store.retrieve(id_b).unwrap();
        assert_eq!(*loaded_a, [0xAA_u8; SEED_LEN]);
        assert_eq!(*loaded_b, [0xBB_u8; SEED_LEN]);
    }

    fn contract_deleting_one_identity_does_not_affect_others(store: &impl OperatorKeyStore) {
        let id_a = IdentityId::new().unwrap();
        let id_b = IdentityId::new().unwrap();
        store.store(id_a, &fresh_seed(0xAA)).unwrap();
        let _guard_a = DeleteOnDrop {
            store,
            identity_id: id_a,
        };
        store.store(id_b, &fresh_seed(0xBB)).unwrap();
        let _guard_b = DeleteOnDrop {
            store,
            identity_id: id_b,
        };
        store.delete(id_a).unwrap();
        assert!(matches!(
            store.retrieve(id_a).unwrap_err(),
            KeychainError::NotFound { .. },
        ));
        let loaded_b = store.retrieve(id_b).unwrap();
        assert_eq!(*loaded_b, [0xBB_u8; SEED_LEN]);
    }

    fn contract_invalid_seed_length_error_displays_lengths(_store: &impl OperatorKeyStore) {
        // Construct the variant directly — no store operation produces it;
        // it requires a corrupt existing entry in the real backends.
        let id = IdentityId::new().unwrap();
        let err = KeychainError::InvalidSeedLength {
            identity_id: id,
            len: 7,
        };
        let rendered = err.to_string();
        assert!(rendered.contains(&id.to_string()));
        assert!(rendered.contains("7 bytes"));
        assert!(rendered.contains("32"));
    }

    fn contract_not_found_error_displays_identity_id(_store: &impl OperatorKeyStore) {
        let id = IdentityId::new().unwrap();
        let err = KeychainError::NotFound { identity_id: id };
        let rendered = err.to_string();
        assert!(rendered.contains(&id.to_string()));
    }

    /// Type-level witness: `OperatorKeyStore::retrieve` returns
    /// `Zeroizing<[u8; SEED_LEN]>`. A refactor returning `[u8; SEED_LEN]` or
    /// `Vec<u8>` would break this.
    fn contract_retrieve_returns_zeroizing_wrapped_seed(store: &impl OperatorKeyStore) {
        let id = IdentityId::new().unwrap();
        store.store(id, &fresh_seed(0x42)).unwrap();
        let _guard = DeleteOnDrop {
            store,
            identity_id: id,
        };
        let loaded: Zeroizing<[u8; SEED_LEN]> = store.retrieve(id).unwrap();
        assert_eq!(loaded.len(), SEED_LEN);
    }

    /// Zero-byte and all-0xFF seeds must round-trip without corruption.
    fn contract_zero_and_max_byte_seeds_round_trip(store: &impl OperatorKeyStore) {
        let id_zero = IdentityId::new().unwrap();
        let id_max = IdentityId::new().unwrap();
        let zero_seed = fresh_seed(0x00);
        let max_seed = fresh_seed(0xFF);
        store.store(id_zero, &zero_seed).unwrap();
        let _guard_zero = DeleteOnDrop {
            store,
            identity_id: id_zero,
        };
        store.store(id_max, &max_seed).unwrap();
        let _guard_max = DeleteOnDrop {
            store,
            identity_id: id_max,
        };
        assert_eq!(*store.retrieve(id_zero).unwrap(), *zero_seed);
        assert_eq!(*store.retrieve(id_max).unwrap(), *max_seed);
    }

    #[test]
    fn memory_store_passes_full_contract_suite() {
        run_contract_suite(&MemoryKeyStore::new());
    }

    #[test]
    fn keychain_error_source_is_none_for_typed_variants() {
        let id = IdentityId::new().unwrap();
        let not_found = KeychainError::NotFound { identity_id: id };
        assert!(std::error::Error::source(&not_found).is_none());

        let bad_len = KeychainError::InvalidSeedLength {
            identity_id: id,
            len: 0,
        };
        assert!(std::error::Error::source(&bad_len).is_none());
    }

    #[test]
    fn keychain_error_debug_does_not_contain_source_debug() {
        let id = IdentityId::new().unwrap();
        let err = KeychainError::NotFound { identity_id: id };
        let rendered = format!("{err:?}");
        assert!(rendered.contains("NotFound"));
        assert!(rendered.contains(&id.to_string()));
    }

    /// Concurrency: two threads storing to the same identity concurrently
    /// must leave exactly one of the two seeds (last-writer-wins).
    #[test]
    fn concurrent_stores_leave_exactly_one_seed() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(MemoryKeyStore::new());
        let id = IdentityId::new().unwrap();
        let seed_a = fresh_seed(0xAA);
        let seed_b = fresh_seed(0xBB);

        let s1 = Arc::clone(&store);
        let s2 = Arc::clone(&store);
        let t1 = thread::spawn(move || s1.store(id, &seed_a).unwrap());
        let t2 = thread::spawn(move || s2.store(id, &seed_b).unwrap());
        t1.join().unwrap();
        t2.join().unwrap();

        let loaded = store.retrieve(id).unwrap();
        // Must be one of the two known seeds, not partial or corrupted.
        assert!(
            *loaded == [0xAA_u8; SEED_LEN] || *loaded == [0xBB_u8; SEED_LEN],
            "retrieved seed is neither of the two stored values",
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn duplicate_entry_display_and_source() {
        let id = IdentityId::new().unwrap();
        let err = KeychainError::DuplicateEntry {
            identity_id: id,
            count: 5,
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains(&id.to_string()),
            "Display missing identity_id"
        );
        assert!(rendered.contains("(5)"), "Display missing count");
        assert!(rendered.contains("cleanup"), "Display missing cleanup hint");
        assert!(
            std::error::Error::source(&err).is_none(),
            "source should be None"
        );
    }

    #[test]
    fn decode_seed_bytes_correct_length_round_trips() {
        let id = IdentityId::new().unwrap();
        let input = [0x42_u8; SEED_LEN];
        let result = decode_seed_bytes(id, &input).unwrap();
        assert_eq!(*result, input);
    }

    #[test]
    fn decode_seed_bytes_wrong_length_returns_invalid_seed_length() {
        let id = IdentityId::new().unwrap();
        let err = decode_seed_bytes(id, &[0u8; 7]).unwrap_err();
        assert!(
            matches!(
                err,
                KeychainError::InvalidSeedLength {
                    identity_id: _,
                    len: 7
                }
            ),
            "expected InvalidSeedLength {{ len: 7 }}, got {err:?}",
        );
    }

    #[test]
    fn decode_seed_bytes_empty_returns_invalid_seed_length() {
        let id = IdentityId::new().unwrap();
        let err = decode_seed_bytes(id, &[]).unwrap_err();
        assert!(
            matches!(
                err,
                KeychainError::InvalidSeedLength {
                    identity_id: _,
                    len: 0
                }
            ),
            "expected InvalidSeedLength {{ len: 0 }}, got {err:?}",
        );
    }

    /// `InvalidSecretEncoding` Display and Debug contain the label but NOT
    /// a sentinel secret value (labels are non-sensitive service identifiers).
    #[test]
    fn invalid_secret_encoding_display_and_debug_contain_label_only() {
        let err = KeychainError::InvalidSecretEncoding {
            label: "reeve-anthropic-api-key".to_owned(),
        };
        let display_output = format!("{err}");
        let debug_output = format!("{err:?}");

        // Label (non-secret) appears in both representations.
        assert!(
            display_output.contains("reeve-anthropic-api-key"),
            "Display missing label: {display_output}",
        );
        assert!(
            debug_output.contains("reeve-anthropic-api-key"),
            "Debug missing label: {debug_output}",
        );

        // Source is None — this variant carries no chained error.
        assert!(
            std::error::Error::source(&err).is_none(),
            "source should be None for InvalidSecretEncoding",
        );
    }

    /// Regression guard: if a future variant change adds a non-label field, the
    /// test name and comment will guide the maintainer to add a
    /// sentinel-NOT-contained assertion for that field.
    #[test]
    fn invalid_secret_encoding_does_not_leak_secret_value() {
        const SECRET_SENTINEL: &str = "test-secret-sentinel-must-not-leak";
        let err = KeychainError::InvalidSecretEncoding {
            label: format!("test-label-with-{SECRET_SENTINEL}-embedded"),
        };
        let display_output = format!("{err}");
        let debug_output = format!("{err:?}");

        assert!(
            display_output.contains("test-label-with-"),
            "label should appear in Display",
        );
        assert!(
            debug_output.contains("test-label-with-"),
            "label should appear in Debug",
        );
        // (No additional assertions needed today; this test is a regression
        // guard for future variant changes that add non-label fields.)
    }
}
