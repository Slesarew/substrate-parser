//! Interpret storage data.
//!
//! Chain storage could be queried by rpc `state_getStorage` using a whole key
//! or a key prefix.
//!
//! Storage key has prefix built from `prefix` of
//! [`PalletStorageMetadata`](frame_metadata::v14::PalletStorageMetadata) and
//! `name` of
//! [`StorageEntryMetadata`](frame_metadata::v14::StorageEntryMetadata), both
//! processed as bytes in [`twox_128`](sp_core_hashing::twox_128) and
//! concatenated together.
//!
//! There are `Plain` and `Map` variants of [`StorageEntryType`].
//!
//! The storage key for `Plain` variant contains only the prefix.
//!
//! The storage key for `Map` has additional data after the prefix that in some
//! cases could be parsed too. For `Map` there are multiple keys with the same
//! prefix.
//!
//! Storage value contains corresponding encoded data with type described in
//! [`StorageEntryType`].
//!
//! [`Storage`] contains parsed storage entry data (key, value, and general
//! documentation associated with every key with the same prefix).
use external_memory_tools::{AddressableBuffer, ExternalMemory};
use frame_metadata::v14::{StorageEntryMetadata, StorageEntryType, StorageHasher};
use scale_info::{form::PortableForm, interner::UntrackedSymbol, TypeDef};
use sp_core_hashing::{blake2_128, twox_64};

use crate::std::{string::String, vec::Vec};

#[cfg(feature = "std")]
use std::any::TypeId;

#[cfg(not(feature = "std"))]
use core::any::TypeId;

use crate::cards::{Documented, ExtendedData, Info};
use crate::decode_all_as_type;
use crate::decoding_sci::{decode_with_type, Ty};
use crate::error::{ParserError, StorageError};
use crate::propagated::Propagated;
use crate::traits::{AsMetadata, ResolveType};

/// Parsed storage entry data: key, value, general docs.
#[derive(Debug, Eq, PartialEq)]
pub struct Storage {
    /// Storage key data.
    pub key: KeyData,

    /// Storage value decoded.
    pub value: ExtendedData,

    /// [`StorageEntryMetadata`](frame_metadata::v14::StorageEntryMetadata)
    /// documentation, common for all storage entries with the same prefix.
    pub docs: String,
}

/// Processed key.
#[derive(Debug, Eq, PartialEq)]
pub enum KeyData {
    /// Plain storage. Key contains only prefix, no additional data.
    Plain,

    /// Map storage with a single [`StorageHasher`].
    ///
    /// Key contains prefix and a single hash. No restrictions on key type.
    SingleHash {
        /// Processed key data.
        content: KeyPart,
    },

    /// Map storage with a set of [`StorageHasher`]s.
    ///
    /// Key contains prefix and a set of hashes. Associated key is expected to
    /// be a tuple type, the number of fields in tuple is exactly the same as
    /// the number of elements in `Vec<StorageHasher>`.
    TupleHash {
        /// Set of processed key elements. Has the same number of elements as
        /// the set of [`StorageHasher`]s.
        content: Vec<KeyPart>,

        /// [`Info`] associated with key type as a whole.
        info: Info,
    },
}

/// Processed key part.
#[derive(Debug, Eq, PartialEq)]
pub enum KeyPart {
    /// Hash has no concatenated raw part, no data decoded.
    Hash(HashData),

    /// Data following hash decoded.
    Parsed(ExtendedData),
}

/// Data for hash that was not decoded.
#[derive(Debug, Eq, PartialEq)]
pub struct HashData {
    /// Hash itself.
    pub hash: Hash,

    /// Associated type id in `PortableRegistry`.
    pub type_id: u32,
}

/// Raw hashes.
#[derive(Debug, Eq, PartialEq)]
pub enum Hash {
    Blake2_128([u8; BLAKE2_128_LEN]),
    Blake2_256([u8; BLAKE2_256_LEN]),
    Twox128([u8; TWOX128_LEN]),
    Twox256([u8; TWOX256_LEN]),
}

/// Lenght in bytes for Blake2_128 hash.
pub const BLAKE2_128_LEN: usize = 16;

/// Lenght in bytes for Blake2_256 hash.
pub const BLAKE2_256_LEN: usize = 32;

/// Lenght in bytes for Twox_128 hash.
pub const TWOX128_LEN: usize = 16;

/// Lenght in bytes for Twox_256 hash.
pub const TWOX256_LEN: usize = 32;

/// Lenght in bytes for Twox_64 hash.
pub const TWOX64_LEN: usize = 8;

/// Parse a storage entry (both the key and the corresponding value).
///
/// The key here is used "as is", i.e. starts with the prefix.
/// Prefix content **is not** checked here.
///
/// Both the key and the value are expected to be processed completely, i.e.
/// with no data remaining.
pub fn decode_as_storage_entry<B, E, M>(
    key_input: &B,
    value_input: &B,
    ext_memory: &mut E,
    entry_metadata: &StorageEntryMetadata<PortableForm>,
    registry: &M::TypeRegistry,
) -> Result<Storage, StorageError<E>>
where
    B: AddressableBuffer<E>,
    E: ExternalMemory,
    M: AsMetadata<E>,
{
    let docs = entry_metadata.collect_docs();

    let position_key_after_prefix = 2 * TWOX128_LEN;
    if position_key_after_prefix > key_input.total_len() {
        return Err(StorageError::KeyShorterThanPrefix);
    }

    let (key, value) = match &entry_metadata.ty {
        StorageEntryType::Plain(value_ty) => {
            if position_key_after_prefix != key_input.total_len() {
                return Err(StorageError::PlainKeyExceedsPrefix);
            }
            let key = KeyData::Plain;
            let value = decode_all_as_type::<B, E, M>(value_ty, value_input, ext_memory, registry)
                .map_err(StorageError::ParsingValue)?;
            (key, value)
        }
        StorageEntryType::Map {
            hashers,
            key: key_ty,
            value: value_ty,
        } => {
            let key = process_key_mapped::<B, E, M>(
                hashers,
                key_ty,
                key_input,
                ext_memory,
                position_key_after_prefix,
                registry,
            )?;
            let value = decode_all_as_type::<B, E, M>(value_ty, value_input, ext_memory, registry)
                .map_err(StorageError::ParsingValue)?;
            (key, value)
        }
    };
    Ok(Storage { key, value, docs })
}

/// Hash processing for hashes with known length and **no** concatenated
/// decodeable part.
///
/// Position moves according to the hash length. No parsing occurs here.
macro_rules! cut_hash {
    ($func:ident, $hash_len:ident, $enum_variant:ident) => {
        fn $func<B, E>(
            key_ty: &UntrackedSymbol<TypeId>,
            key_input: &B,
            ext_memory: &mut E,
            position: &mut usize,
        ) -> Result<KeyPart, StorageError<E>>
        where
            B: AddressableBuffer<E>,
            E: ExternalMemory,
        {
            let slice = key_input
                .read_slice(ext_memory, *position, $hash_len)
                .map_err(|e| StorageError::ParsingKey(ParserError::Buffer(e)))?;
            let hash_part: [u8; $hash_len] = slice
                .as_ref()
                .try_into()
                .expect("constant length, always fits");
            *position += $hash_len;
            Ok(KeyPart::Hash(HashData {
                hash: Hash::$enum_variant(hash_part),
                type_id: key_ty.id,
            }))
        }
    };
}

cut_hash!(cut_blake2_128, BLAKE2_128_LEN, Blake2_128);
cut_hash!(cut_blake2_256, BLAKE2_256_LEN, Blake2_256);
cut_hash!(cut_twox_128, TWOX128_LEN, Twox128);
cut_hash!(cut_twox_256, TWOX256_LEN, Twox256);

/// Hash processing for hashes with known length and **added** concatenated
/// decodeable part.
///
/// Position moves according to the hash length. Data parsing starts at first
/// byte after the hash. `&[u8]` slice corresponding to the parsed data is
/// hashed and matched with the hash found in the key.
macro_rules! check_hash {
    ($func:ident, $hash_len:ident, $fn_into:ident) => {
        fn $func<B, E, M>(
            ty: &UntrackedSymbol<TypeId>,
            key_input: &B,
            ext_memory: &mut E,
            position: &mut usize,
            registry: &M::TypeRegistry,
        ) -> Result<KeyPart, StorageError<E>>
        where
            B: AddressableBuffer<E>,
            E: ExternalMemory,
            M: AsMetadata<E>,
        {
            let slice = key_input
                .read_slice(ext_memory, *position, $hash_len)
                .map_err(|e| StorageError::ParsingKey(ParserError::Buffer(e)))?;
            let hash_part: [u8; $hash_len] = slice
                .as_ref()
                .try_into()
                .expect("constant length, always fits");
            *position += $hash_len;
            let position_decoder_starts = *position;
            let parsed_key = decode_with_type::<B, E, M>(
                &Ty::Symbol(&ty),
                key_input,
                ext_memory,
                position,
                registry,
                Propagated::new(),
            )
            .map_err(StorageError::ParsingKey)?;
            if hash_part
                != $fn_into(
                    &key_input
                        .read_slice(
                            ext_memory,
                            position_decoder_starts,
                            *position - position_decoder_starts,
                        )
                        .expect("positions checked, valid slice")
                        .as_ref(),
                )
            {
                Err(StorageError::KeyPartHashMismatch)
            } else {
                Ok(KeyPart::Parsed(parsed_key))
            }
        }
    };
}

check_hash!(check_blake2_128, BLAKE2_128_LEN, blake2_128);
check_hash!(check_twox_64, TWOX64_LEN, twox_64);

/// Process the storage key data for `StorageEntryType::Map{..}` keys.
///
/// Starting position is explicitly set as a variable. For "as is" storage key
/// the starting position must be 2*[`TWOX128_LEN`], for trimmed storage key the
/// starting position must be `0`.
///
/// Key is expected to get processed completely, i.e. with no data remaining.
pub fn process_key_mapped<B, E, M>(
    hashers: &[StorageHasher],
    key_ty: &UntrackedSymbol<TypeId>,
    key_input: &B,
    ext_memory: &mut E,
    mut position: usize,
    registry: &M::TypeRegistry,
) -> Result<KeyData, StorageError<E>>
where
    B: AddressableBuffer<E>,
    E: ExternalMemory,
    M: AsMetadata<E>,
{
    let key_data = {
        if hashers.len() == 1 {
            match hashers[0] {
                StorageHasher::Blake2_128 => KeyData::SingleHash {
                    content: cut_blake2_128::<B, E>(key_ty, key_input, ext_memory, &mut position)?,
                },
                StorageHasher::Blake2_256 => KeyData::SingleHash {
                    content: cut_blake2_256::<B, E>(key_ty, key_input, ext_memory, &mut position)?,
                },
                StorageHasher::Blake2_128Concat => KeyData::SingleHash {
                    content: check_blake2_128::<B, E, M>(
                        key_ty,
                        key_input,
                        ext_memory,
                        &mut position,
                        registry,
                    )?,
                },
                StorageHasher::Twox128 => KeyData::SingleHash {
                    content: cut_twox_128::<B, E>(key_ty, key_input, ext_memory, &mut position)?,
                },
                StorageHasher::Twox256 => KeyData::SingleHash {
                    content: cut_twox_256::<B, E>(key_ty, key_input, ext_memory, &mut position)?,
                },
                StorageHasher::Twox64Concat => KeyData::SingleHash {
                    content: check_twox_64::<B, E, M>(
                        key_ty,
                        key_input,
                        ext_memory,
                        &mut position,
                        registry,
                    )?,
                },
                StorageHasher::Identity => {
                    let parsed_key = decode_with_type::<B, E, M>(
                        &Ty::Symbol(key_ty),
                        key_input,
                        ext_memory,
                        &mut position,
                        registry,
                        Propagated::new(),
                    )
                    .map_err(StorageError::ParsingKey)?;
                    KeyData::SingleHash {
                        content: KeyPart::Parsed(parsed_key),
                    }
                }
            }
        } else {
            let key_ty_resolved = registry
                .resolve_ty(key_ty.id, ext_memory)
                .map_err(StorageError::ParsingKey)?;
            let info = Info::from_ty(&key_ty_resolved);
            match key_ty_resolved.type_def {
                TypeDef::Tuple(t) => {
                    let tuple_elements = &t.fields;
                    if tuple_elements.len() != hashers.len() {
                        return Err(StorageError::MultipleHashesNumberMismatch);
                    }
                    let mut content: Vec<KeyPart> = Vec::new();
                    for index in 0..tuple_elements.len() {
                        match hashers[index] {
                            StorageHasher::Blake2_128 => content.push(cut_blake2_128::<B, E>(
                                &tuple_elements[index],
                                key_input,
                                ext_memory,
                                &mut position,
                            )?),
                            StorageHasher::Blake2_256 => content.push(cut_blake2_256::<B, E>(
                                &tuple_elements[index],
                                key_input,
                                ext_memory,
                                &mut position,
                            )?),
                            StorageHasher::Blake2_128Concat => {
                                content.push(check_blake2_128::<B, E, M>(
                                    &tuple_elements[index],
                                    key_input,
                                    ext_memory,
                                    &mut position,
                                    registry,
                                )?)
                            }
                            StorageHasher::Twox128 => content.push(cut_twox_128::<B, E>(
                                &tuple_elements[index],
                                key_input,
                                ext_memory,
                                &mut position,
                            )?),
                            StorageHasher::Twox256 => content.push(cut_twox_256::<B, E>(
                                &tuple_elements[index],
                                key_input,
                                ext_memory,
                                &mut position,
                            )?),
                            StorageHasher::Twox64Concat => content.push(check_twox_64::<B, E, M>(
                                &tuple_elements[index],
                                key_input,
                                ext_memory,
                                &mut position,
                                registry,
                            )?),
                            StorageHasher::Identity => {
                                let parsed_key = decode_with_type::<B, E, M>(
                                    &Ty::Symbol(&tuple_elements[index]),
                                    key_input,
                                    ext_memory,
                                    &mut position,
                                    registry,
                                    Propagated::new(),
                                )
                                .map_err(StorageError::ParsingKey)?;
                                content.push(KeyPart::Parsed(parsed_key))
                            }
                        }
                    }
                    KeyData::TupleHash { content, info }
                }
                _ => return Err(StorageError::MultipleHashesNotATuple),
            }
        }
    };
    if position == key_input.total_len() {
        Ok(key_data)
    } else {
        Err(StorageError::KeyPartsUnused)
    }
}
