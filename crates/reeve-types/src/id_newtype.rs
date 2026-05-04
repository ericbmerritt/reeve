//! Declarative macro for `UUIDv7`-backed identifier newtypes.
//!
//! Consolidates the boilerplate shared by every UUIDv7-backed identifier
//! (transparent `Uuid` wrapper, minting constructor, v7-rejecting `TryFrom`,
//! delegating serde, parallel error enum) into a single declaration. The
//! macro uses `$crate::uuid_v7` and is intended to be invoked from within
//! `reeve-types`.

/// Generate a `UUIDv7`-backed newtype identifier and its error enum.
///
/// See the module docs for the full surface generated. Both `$Name` and
/// `$Error` are emitted with `pub` visibility; place the invocation in the
/// module that should own the type.
macro_rules! uuid_v7_newtype {
    (
        $(#[$type_meta:meta])*
        pub $Name:ident,
        $(#[$err_meta:meta])*
        error $Error:ident,
        noun $noun:literal $(,)?
    ) => {
        $(#[$type_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $Name(::uuid::Uuid);

        impl $Name {
            #[doc = concat!(
                "Mint a fresh `UUIDv7` ", $noun,
                ". Returns [`", stringify!($Error),
                "::ClockBeforeEpoch`] if the host clock is set before 1970-01-01 UTC."
            )]
            pub fn new() -> ::core::result::Result<Self, $Error> {
                let uuid = $crate::uuid_v7::now_v7().map_err($Error::from)?;
                ::core::result::Result::Ok(Self(uuid))
            }

            pub fn as_uuid(&self) -> &::uuid::Uuid {
                &self.0
            }
        }

        impl ::core::fmt::Display for $Name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl ::core::convert::TryFrom<::uuid::Uuid> for $Name {
            type Error = $Error;

            fn try_from(uuid: ::uuid::Uuid) -> ::core::result::Result<Self, Self::Error> {
                let version = uuid.get_version_num();
                if version == 7 {
                    ::core::result::Result::Ok(Self(uuid))
                } else {
                    ::core::result::Result::Err($Error::NotV7 {
                        actual_version: version,
                    })
                }
            }
        }

        impl ::serde::Serialize for $Name {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                self.0.serialize(serializer)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $Name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                let uuid = ::uuid::Uuid::deserialize(deserializer)?;
                <Self as ::core::convert::TryFrom<::uuid::Uuid>>::try_from(uuid)
                    .map_err(::serde::de::Error::custom)
            }
        }

        $(#[$err_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $Error {
            /// Rejected at the parse boundary because the spec mandates
            /// `UUIDv7` for all identifiers (domain-model § Identifiers); a
            /// non-v7 wire form would silently disable chronological
            /// sortability and the v7-implied uniqueness model.
            NotV7 { actual_version: usize },

            /// Surfaced when the host clock is misconfigured to a pre-1970
            /// value; `UUIDv7` cannot encode pre-epoch instants and the
            /// runtime refuses to mint IDs against an unreliable clock rather
            /// than coerce to zero.
            ClockBeforeEpoch,
        }

        impl ::core::fmt::Display for $Error {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                match self {
                    Self::NotV7 { actual_version } => {
                        write!(
                            f,
                            concat!($noun, " must be UUIDv7: got UUIDv{}"),
                            actual_version,
                        )
                    }
                    Self::ClockBeforeEpoch => {
                        f.write_str("system clock is before the Unix epoch")
                    }
                }
            }
        }

        impl ::std::error::Error for $Error {}

        impl ::core::convert::From<$crate::uuid_v7::UuidV7Error> for $Error {
            fn from(value: $crate::uuid_v7::UuidV7Error) -> Self {
                match value {
                    $crate::uuid_v7::UuidV7Error::ClockBeforeEpoch => Self::ClockBeforeEpoch,
                }
            }
        }
    };
}

pub(crate) use uuid_v7_newtype;
