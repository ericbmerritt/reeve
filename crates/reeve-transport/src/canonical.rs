//! Canonical JSON serialization for envelope signing per RFC 8785 (JCS).
//!
//! The [`canonical_bytes`] function produces the deterministic byte
//! representation that the sender signs and the verifier checks. The
//! `signature` field is excluded; including it would require the signature
//! over bytes that themselves contain the signature — a circularity.
//!
//! Canonicalization rules (RFC 8785 §3.2):
//! - Object keys sorted by UTF-16 code-unit order (ASCII field names sort
//!   identically under both UTF-16 and UTF-8 lexicographic order).
//! - No insignificant whitespace.
//! - Numbers: `serde_json` emits integers without decimal point or exponent,
//!   and our envelope carries no float fields, so number normalization is
//!   satisfied automatically.
//! - Strings: `serde_json`'s minimal Unicode-escape output meets RFC 8785.
//! - Output is UTF-8.

use std::fmt;

use serde_json::Value;

use reeve_types::Envelope;

/// Errors that can occur while producing canonical bytes.
///
/// In practice the only failure mode is an internal `serde_json` serialization
/// error, which cannot occur for a well-typed [`Envelope`] (no float specials,
/// no non-string map keys). The error path exists for type-system completeness.
#[derive(Debug)]
#[non_exhaustive]
pub enum CanonicalError {
    /// `serde_json` failed to serialize the envelope fields.
    Serialize(serde_json::Error),
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(err) => write!(f, "canonical serialization failed: {err}"),
        }
    }
}

impl std::error::Error for CanonicalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialize(err) => Some(err),
        }
    }
}

/// Sort all `Object` keys in a [`Value`] tree into RFC 8785 lexicographic
/// order (ascending by UTF-16 code-unit sequence, which equals UTF-8
/// lexicographic order for the all-ASCII field names in our envelope).
fn sort_json_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted: Vec<(String, Value)> = map
                .into_iter()
                .map(|(k, v)| (k, sort_json_value(v)))
                .collect();
            sorted.sort_by(|(a, _), (b, _)| a.cmp(b));
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_json_value).collect()),
        // FLOAT GUARD: Envelope today has only u32 (schema_version). Adding a
        // float field requires explicit RFC 8785 number-normalization handling
        // here; serde_json's default float emission is not JCS-compliant.
        other @ (Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)) => other,
    }
}

/// Produce the canonical JSON bytes for `envelope` per RFC 8785 / JCS.
///
/// The `signature` field is excluded. Verification re-derives these same bytes
/// from the received envelope (excluding signature) and checks the signature
/// against them. This is the only byte representation the verifier ever acts
/// on; it never re-serializes from an intermediate form.
///
/// Serialization goes through `Envelope`'s own `Serialize` impl so that the
/// canonical encoding is guaranteed to match the wire encoding for every field.
/// The `signature` key is removed from the intermediate `Value` before sorting
/// and emitting.
///
/// # Errors
///
/// Returns [`CanonicalError::Serialize`] if `serde_json` fails. For
/// well-typed [`Envelope`] values this is unreachable in practice.
pub fn canonical_bytes(envelope: &Envelope) -> Result<Vec<u8>, CanonicalError> {
    let mut value = serde_json::to_value(envelope).map_err(CanonicalError::Serialize)?;
    if let Value::Object(ref mut map) = value {
        // Field name must match Envelope's serde serialization key. Tests gate
        // drift if Envelope ever renames via #[serde(rename = ...)].
        map.remove("signature");
    }
    let sorted = sort_json_value(value);
    serde_json::to_vec(&sorted).map_err(CanonicalError::Serialize)
}

#[cfg(test)]
mod tests {
    use super::*;

    use time::macros::datetime;

    use reeve_types::{
        EnvelopeSignature, IdentityId, KeyId, MessageId, Nonce, PayloadHash, SchemaVersion,
        NONCE_LEN, PAYLOAD_HASH_LEN, SIGNATURE_LEN,
    };

    fn fixed_envelope() -> Envelope {
        // All inputs are deterministic constants — no wall clock, no random.
        use uuid::Uuid;

        let msg_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000001").unwrap();
        let sender_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000002").unwrap();
        let key_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000003").unwrap();
        let recipient_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000004").unwrap();

        let message_id = MessageId::try_from(msg_uuid).unwrap();
        let sender_id = IdentityId::try_from(sender_uuid).unwrap();
        let sender_key_id = KeyId::try_from(key_uuid).unwrap();
        let recipient_id = IdentityId::try_from(recipient_uuid).unwrap();

        let nonce = Nonce::from_bytes([0xAA; NONCE_LEN]);
        let payload_hash = PayloadHash::from_bytes([0xBB; PAYLOAD_HASH_LEN]);
        let signature = EnvelopeSignature::from_bytes([0xCC; SIGNATURE_LEN]);

        Envelope::new(
            SchemaVersion::V1,
            message_id,
            sender_id,
            sender_key_id,
            recipient_id,
            datetime!(2026-05-04 00:00:00 UTC),
            nonce,
            payload_hash,
            b"hello reeve".to_vec(),
            signature,
        )
    }

    #[test]
    fn canonical_bytes_is_stable_across_calls() {
        let env = fixed_envelope();
        let a = canonical_bytes(&env).unwrap();
        let b = canonical_bytes(&env).unwrap();
        assert_eq!(
            a, b,
            "same envelope must produce identical bytes every call"
        );
    }

    #[test]
    fn canonical_bytes_golden() {
        // Fixed inputs produce a fixed byte string. Any encoding drift
        // (base64 alphabet change, timestamp format change, etc.) breaks this.
        let env = fixed_envelope();
        let bytes = canonical_bytes(&env).unwrap();
        let json = std::str::from_utf8(&bytes).unwrap();

        // The expected literal is derived from the fixed inputs above.
        // body = b"hello reeve" → unpadded base64 = "aGVsbG8gcmVldmU"
        // nonce = [0xAA; 16] → "qqqqqqqqqqqqqqqqqqqqqg"
        // payload_hash = [0xBB; 32] → "u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7s"
        // message_id  = 01968e40-0000-7000-8000-000000000001 (hyphenated)
        // sender_id   = 01968e40-0000-7000-8000-000000000002
        // sender_key_id = 01968e40-0000-7000-8000-000000000003
        // recipient_id  = 01968e40-0000-7000-8000-000000000004
        // created_at  = "2026-05-04T00:00:00Z"
        // schema_version = 1
        let expected = concat!(
            r#"{"body":"aGVsbG8gcmVldmU","created_at":"2026-05-04T00:00:00Z","#,
            r#""message_id":"01968e40-0000-7000-8000-000000000001","#,
            r#""nonce":"qqqqqqqqqqqqqqqqqqqqqg","#,
            r#""payload_hash":"u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7u7s","#,
            r#""recipient_id":"01968e40-0000-7000-8000-000000000004","#,
            r#""schema_version":1,"#,
            r#""sender_id":"01968e40-0000-7000-8000-000000000002","#,
            r#""sender_key_id":"01968e40-0000-7000-8000-000000000003"}"#,
        );
        assert_eq!(json, expected, "canonical bytes must match golden literal");
    }

    #[test]
    fn canonical_bytes_keys_are_alphabetically_ordered() {
        let env = fixed_envelope();
        let bytes = canonical_bytes(&env).unwrap();
        let json = std::str::from_utf8(&bytes).unwrap();

        let expected_order = [
            "body",
            "created_at",
            "message_id",
            "nonce",
            "payload_hash",
            "recipient_id",
            "schema_version",
            "sender_id",
            "sender_key_id",
        ];

        let mut last_pos = 0usize;
        for (i, key) in expected_order.iter().enumerate() {
            let needle = format!("\"{key}\":");
            let pos = json
                .find(needle.as_str())
                .unwrap_or_else(|| panic!("key {key} not found in canonical JSON: {json}"));
            if i > 0 {
                assert!(
                    pos > last_pos,
                    "key {key} at position {pos} is not strictly after previous at {last_pos}: {json}"
                );
            }
            last_pos = pos;
        }
    }

    #[test]
    fn canonical_bytes_contains_no_insignificant_whitespace() {
        let env = fixed_envelope();
        let bytes = canonical_bytes(&env).unwrap();
        let json = std::str::from_utf8(&bytes).unwrap();
        let value: Value = serde_json::from_str(json).unwrap();
        let compact = serde_json::to_string(&value).unwrap();
        assert_eq!(
            json, compact,
            "canonical bytes must be the compact JSON form"
        );
    }

    #[test]
    fn canonical_bytes_excludes_signature_field() {
        let env = fixed_envelope();
        let bytes = canonical_bytes(&env).unwrap();
        let json = std::str::from_utf8(&bytes).unwrap();
        assert!(
            !json.contains("\"signature\""),
            "canonical bytes must not contain the signature field: {json}"
        );
        // 0xCC repeated base64-encodes with this fragment — not present if signature is excluded.
        let sig_b64_fragment = "zMzM";
        assert!(
            !json.contains(sig_b64_fragment),
            "canonical bytes must not contain signature base64: {json}"
        );
    }

    #[test]
    fn canonical_bytes_schema_version_is_integer_one() {
        let env = fixed_envelope();
        let bytes = canonical_bytes(&env).unwrap();
        let json = std::str::from_utf8(&bytes).unwrap();
        assert!(
            json.contains("\"schema_version\":1"),
            "schema_version must serialize as integer 1: {json}"
        );
        assert!(
            !json.contains("\"schema_version\":1.0"),
            "schema_version must not have decimal point: {json}"
        );
    }

    #[test]
    fn canonical_bytes_re_canonicalized_equals_original() {
        // Stability-under-reserialization verifies the spec's "never on a
        // reserialized form" invariant: canonical_bytes of the canonical form
        // is identical to the canonical form itself.
        let env = fixed_envelope();
        let first = canonical_bytes(&env).unwrap();
        let value: Value = serde_json::from_slice(&first).unwrap();
        let sorted = sort_json_value(value);
        let second = serde_json::to_vec(&sorted).unwrap();
        assert_eq!(
            first, second,
            "canonical form must be stable under re-canonicalization"
        );
    }

    #[test]
    fn different_envelopes_produce_different_bytes() {
        let env_a = fixed_envelope();
        let mut env_b = fixed_envelope();
        env_b.body = b"different body".to_vec();
        let a = canonical_bytes(&env_a).unwrap();
        let b = canonical_bytes(&env_b).unwrap();
        assert_ne!(a, b, "distinct envelopes must produce distinct bytes");
    }

    #[test]
    fn canonical_bytes_body_is_base64_encoded() {
        let env = fixed_envelope();
        let bytes = canonical_bytes(&env).unwrap();
        let json = std::str::from_utf8(&bytes).unwrap();
        // "hello reeve" unpadded standard base64
        assert!(
            json.contains("aGVsbG8gcmVldmU"),
            "body must appear as unpadded base64: {json}"
        );
    }

    #[test]
    fn canonical_bytes_created_at_is_utc_z_form() {
        let env = fixed_envelope();
        let bytes = canonical_bytes(&env).unwrap();
        let json = std::str::from_utf8(&bytes).unwrap();
        assert!(
            json.contains("+00:00") || json.contains('Z'),
            "created_at must be in UTC form: {json}"
        );
    }

    #[test]
    fn canonical_error_display_and_source() {
        use std::error::Error as _;
        let raw_err: serde_json::Error = serde_json::from_str::<Value>("not json").unwrap_err();
        let err = CanonicalError::Serialize(raw_err);
        let msg = err.to_string();
        assert!(
            msg.contains("canonical serialization failed"),
            "unexpected display: {msg}"
        );
        assert!(
            err.source().is_some(),
            "CanonicalError::Serialize must expose its source"
        );
    }
}
