//! Decode unchecked extrinsics, signed or unsigned.
//!
//! Here in decoding it is assumed that the unchecked extrinsic is an encoded
//! opaque `Vec<u8>`, its general structure described
//! [here](https://docs.substrate.io/reference/transaction-format/).
//!
//! Unchecked extrinsic type [`UncheckedExtrinsic`](https://docs.rs/sp-runtime/latest/sp_runtime/generic/struct.UncheckedExtrinsic.html)
//! is metadata-specific, and is described in
//! [`ExtrinsicMetadata`](https://docs.rs/frame-metadata/15.0.0/frame_metadata/v14/struct.ExtrinsicMetadata.html).
//! There `ty` is the extrinsic type, which is expected to resolve into
//! `Vec<u8>`, and its parameters specify the address, signature, extra data,
//! and call types, from which the extrinsic is built.
//!
//! Signed unchecked extrinsic structure:
//!
//! <table>
//!   <tr>
//!     <td>compact length of whole extrinsic</td>
//!     <td>version byte</td>
//!     <td>address that produced the extrinsic</td>
//!     <td>signature</td>
//!     <td>extra data</td>
//!     <td>call</td>
//!   </tr>
//! </table>
//!
//! Unsigned unchecked extrinsic structure:
//!
//! <table>
//!   <tr>
//!     <td>compact length of whole extrinsic</td>
//!     <td>version byte</td>
//!     <td>call</td>
//!   </tr>
//! </table>
//!
//! Signed and unsigned unchecked extrinsics are differentiated by version byte.
//! The first bit in the version byte is `0` if the extrinsic is unsigned and
//! `1` if the extrinsic is signed. Other 7 bits must match the extrinsic
//! `version` from [`ExtrinsicMetadata`](https://docs.rs/frame-metadata/15.0.0/frame_metadata/v14/struct.ExtrinsicMetadata.html).
//! Currently the `version` has a constant value of `4` (see
//! [`EXTRINSIC_FORMAT_VERSION`](https://docs.rs/sp-runtime/9.0.0/src/sp_runtime/generic/unchecked_extrinsic.rs.html#39)
//! in `sp_runtime`, thus version byte is `0x04` for unsigned extrinsics and
//! `0x84` for signed extrinsics.
#[cfg(feature = "std")]
use std::cmp::Ordering;

#[cfg(not(feature = "std"))]
use core::cmp::Ordering;

use external_memory_tools::{AddressableBuffer, BufferError, ExternalMemory};

use crate::cards::{Call, ExtendedData, ParsedData};
use crate::compacts::get_compact;
use crate::decode_as_type_at_position;
use crate::decoding_sci::{extrinsic_type_params, CALL_INDICATOR};
use crate::error::{ParserError, UncheckedExtrinsicError};
use crate::traits::AsMetadata;

/// Length of version indicator, 1 byte.
const VERSION_LENGTH: usize = 1;

/// Version byte mask, to separate version and signed/unsigned information.
const VERSION_MASK: u8 = 0b0111_1111;

/// Version value for unsigned extrinsic, after `VERSION_MASK` is applied.
const VERSION_UNSIGNED: u8 = 0;

/// [`TypeParameter`](scale_info::TypeParameter) name for `address`.
const ADDRESS_INDICATOR: &str = "Address";

/// [`TypeParameter`](scale_info::TypeParameter) name for `signature`.
const SIGNATURE_INDICATOR: &str = "Signature";

/// [`TypeParameter`](scale_info::TypeParameter) name for `extra`.
const EXTRA_INDICATOR: &str = "Extra";

/// Decode an unchecked extrinsic.
pub fn decode_as_unchecked_extrinsic<B, E, M>(
    input: &B,
    ext_memory: &mut E,
    meta_v14: &M,
) -> Result<UncheckedExtrinsic, UncheckedExtrinsicError<E, M>>
where
    B: AddressableBuffer<E>,
    E: ExternalMemory,
    M: AsMetadata<E>,
{
    let extrinsic = meta_v14
        .extrinsic()
        .map_err(UncheckedExtrinsicError::MetaStructure)?;
    let extrinsic_ty = extrinsic.ty;
    let meta_v14_types = meta_v14.types();

    let extrinsic_type_params =
        extrinsic_type_params::<E, M>(ext_memory, &meta_v14_types, &extrinsic_ty)?;

    // This could have been just a single decode line.
    // Written this way to: (1) trace position from the start, (2) have descriptive errors
    let mut extrinsic_start: usize = 0;
    let extrinsic_length = get_compact::<u32, B, E>(input, ext_memory, &mut extrinsic_start)
        .map_err(|_| UncheckedExtrinsicError::FormatNoCompact)? as usize;
    let len = input.total_len();
    match (extrinsic_start + extrinsic_length).cmp(&len) {
        Ordering::Greater => {
            return Err(UncheckedExtrinsicError::Parsing(ParserError::Buffer(
                BufferError::DataTooShort {
                    position: len,
                    minimal_length: extrinsic_start + extrinsic_length - len,
                },
            )))
        }
        Ordering::Less => {
            return Err(UncheckedExtrinsicError::Parsing(
                ParserError::SomeDataNotUsedBlob {
                    from: extrinsic_start + extrinsic_length,
                },
            ))
        }
        Ordering::Equal => {}
    }

    let mut position = extrinsic_start;

    // version byte from extrinsic, to diffirentiate signed and unsigned extrinsics
    let version_byte = input
        .read_byte(ext_memory, position)
        .map_err(|e| UncheckedExtrinsicError::Parsing(ParserError::Buffer(e)))?;
    position += VERSION_LENGTH;

    let version = extrinsic.version;

    // First bit of `version_byte` is `0` if the transaction is unsigned and `1` if the transaction is signed.
    // Other 7 bits must match the `version` from the metadata.
    if version_byte & VERSION_MASK != version {
        Err(UncheckedExtrinsicError::VersionMismatch {
            version_byte,
            version,
        })
    } else if version_byte & !VERSION_MASK == VERSION_UNSIGNED {
        let mut found_call = None;

        // Need the call parameter from unchecked extrinsic parameters.
        for param in extrinsic_type_params.iter() {
            if param.name == CALL_INDICATOR {
                found_call = param.ty
            }
        }

        let call_ty = found_call.ok_or(UncheckedExtrinsicError::Parsing(
            ParserError::ExtrinsicNoCallParam,
        ))?;
        let call_extended_data = decode_as_type_at_position::<B, E, M>(
            &call_ty,
            input,
            ext_memory,
            &meta_v14_types,
            &mut position,
        )?;
        if let ParsedData::Call(call) = call_extended_data.data {
            Ok(UncheckedExtrinsic::Unsigned { call })
        } else {
            Err(UncheckedExtrinsicError::UnexpectedCallTy {
                call_ty_id: call_ty.id,
            })
        }
    } else {
        let mut found_address = None;
        let mut found_signature = None;
        let mut found_extra = None;
        let mut found_call = None;

        // Unchecked extrinsic parameters typically contain address, signature,
        // and extensions. Expect to find all entries.
        for param in extrinsic_type_params.iter() {
            match param.name.as_str() {
                ADDRESS_INDICATOR => found_address = param.ty,
                SIGNATURE_INDICATOR => found_signature = param.ty,
                EXTRA_INDICATOR => found_extra = param.ty,
                CALL_INDICATOR => found_call = param.ty,
                _ => (),
            }
        }

        let address_ty = found_address.ok_or(UncheckedExtrinsicError::NoAddressParam)?;
        let address = decode_as_type_at_position::<B, E, M>(
            &address_ty,
            input,
            ext_memory,
            &meta_v14_types,
            &mut position,
        )?;

        let signature_ty = found_signature.ok_or(UncheckedExtrinsicError::NoSignatureParam)?;
        let signature = decode_as_type_at_position::<B, E, M>(
            &signature_ty,
            input,
            ext_memory,
            &meta_v14_types,
            &mut position,
        )?;

        let extra_ty = found_extra.ok_or(UncheckedExtrinsicError::NoExtraParam)?;
        let extra = decode_as_type_at_position::<B, E, M>(
            &extra_ty,
            input,
            ext_memory,
            &meta_v14_types,
            &mut position,
        )?;

        let call_ty = found_call.ok_or(UncheckedExtrinsicError::NoCallParam)?;
        let call_extended_data = decode_as_type_at_position::<B, E, M>(
            &call_ty,
            input,
            ext_memory,
            &meta_v14_types,
            &mut position,
        )?;
        if let ParsedData::Call(call) = call_extended_data.data {
            Ok(UncheckedExtrinsic::Signed {
                address,
                signature,
                extra,
                call,
            })
        } else {
            Err(UncheckedExtrinsicError::UnexpectedCallTy {
                call_ty_id: call_ty.id,
            })
        }
    }
}

/// Decoded unchecked extrinsic.
#[derive(Debug, Eq, PartialEq)]
pub enum UncheckedExtrinsic {
    Signed {
        address: ExtendedData,
        signature: ExtendedData,
        extra: ExtendedData,
        call: Call,
    },
    Unsigned {
        call: Call,
    },
}
