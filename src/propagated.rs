//! Data that can propagate hierarchically during parsing.
use external_memory_tools::ExternalMemory;
use frame_metadata::v14::SignedExtensionMetadata;
use scale_info::{form::PortableForm, Field, Path, Type};

use crate::std::vec::Vec;

use crate::cards::Info;
use crate::error::ParserError;
use crate::special_indicators::{Hint, SpecialtyH256, SpecialtyUnsignedInteger};

/// Type specialty data (type specialty [`Hint`] and compact info) that
/// hierarchically propagates during the decoding.
///
/// Compact flag impacts decoding. [`Hint`] can determine only the decoded data
/// further processing.
#[derive(Clone, Copy, Debug)]
pub struct SpecialtySet {
    /// Compact info.
    ///
    /// `Some(id)` if the parser has encountered type with definition
    /// `TypeDef::Compact(_)` for this `SpecialtySet` instance.
    ///
    /// `id` corresponds to type id in metadata types `Registry`.
    ///
    /// Once `Some(_)`, never changes back to `None`.
    ///
    /// Must cause parser error if `Some(_)`, but the type inside compact has no
    /// [`HasCompact`](parity_scale_codec::HasCompact) implementation.
    ///
    /// Currently is allowed to be `Some(_)` for unsigned integers and
    /// single-field structs with unsigned integer as a field.
    pub compact_at: Option<u32>,

    /// `Hint` the parser has encountered for this `SpecialtySet` instance.
    ///
    /// Does not cause parser errors even if resolved type is incompatible.
    ///
    /// Could be nullified if no longer relevant, i.e. if passed through some
    /// type that is hint-incompatible.
    pub hint: Hint,
}

impl SpecialtySet {
    /// Initiate new `SpecialtySet`.
    pub fn new() -> Self {
        Self {
            compact_at: None,
            hint: Hint::None,
        }
    }

    /// Check that `compact_at` field is not `Some(_)`, i.e. there was no
    /// compact type encountered.
    pub fn reject_compact<E: ExternalMemory>(&self) -> Result<(), ParserError<E>> {
        if let Some(id) = self.compact_at {
            Err(ParserError::UnexpectedCompactInsides { id })
        } else {
            Ok(())
        }
    }

    /// Update `Hint` from type path, if no hint existed previously.
    pub fn update_from_path(&mut self, path: &Path<PortableForm>) {
        if let Hint::None = self.hint {
            self.hint = Hint::from_path(path);
        }
    }

    /// Previously found `Hint` (if there was any) is no longer relevant and is
    /// discarded.
    pub fn forget_hint(&mut self) {
        self.hint = Hint::None;
    }

    /// Apply `hint` field on unsigned integer decoding.
    pub fn unsigned_integer(&self) -> SpecialtyUnsignedInteger {
        self.hint.unsigned_integer()
    }

    /// Apply `hint` field on `H256` decoding.
    pub fn hash256(&self) -> SpecialtyH256 {
        self.hint.hash256()
    }
}

impl Default for SpecialtySet {
    fn default() -> Self {
        Self::new()
    }
}

/// Types data collected and checked during parsing ([`SpecialtySet`] and
/// type id collection to prevent cycling).
#[derive(Clone, Debug)]
pub struct Checker {
    /// `SpecialtySet` initiated new and modified during decoding.
    pub specialty_set: SpecialtySet,

    /// Collection of encountered so far during decoding type `id`s from
    /// metadata types `Registry`, to catch possible endless type resolver
    /// cycles.
    pub cycle_check: Vec<u32>,
}

impl Checker {
    /// Initiate new `Checker` in decoding sequence.
    pub fn new() -> Self {
        Self {
            specialty_set: SpecialtySet::new(),
            cycle_check: Vec::new(),
        }
    }

    /// Check that `compact_at` field in associated [`SpecialtySet`] is not
    /// `Some(_)`, i.e. there was no compact type encountered.
    pub fn reject_compact<E: ExternalMemory>(&self) -> Result<(), ParserError<E>> {
        self.specialty_set.reject_compact::<E>()
    }

    /// Discard previously found [`Hint`].
    pub fn forget_hint(&mut self) {
        self.specialty_set.forget_hint()
    }

    /// Use known, propagated from above `Checker` to construct a new `Checker`
    /// for an individual [`Field`].
    pub fn update_for_field<E: ExternalMemory>(
        &self,
        field: &Field<PortableForm>,
    ) -> Result<Self, ParserError<E>> {
        let mut checker = self.clone();

        // update `Hint`
        if let Hint::None = checker.specialty_set.hint {
            checker.specialty_set.hint = Hint::from_field(field);
        }

        // check that `id` is not cycling and update `id` set
        checker.check_id(field.ty.id)?;

        Ok(checker)
    }

    /// Use known, propagated from above `Checker` to construct a new `Checker`
    /// for a [`Type`].
    pub fn update_for_ty<E: ExternalMemory>(
        &self,
        ty: &Type<PortableForm>,
        id: u32,
    ) -> Result<Self, ParserError<E>> {
        let mut checker = self.clone();
        checker.check_id(id)?;
        checker.specialty_set.update_from_path(&ty.path);
        Ok(checker)
    }

    /// Discard previously collected `cycle_check` set.
    ///
    /// For cases when `Checker` keeps propagating, but decoded data itself has
    /// changed.
    pub fn drop_cycle_check(&mut self) {
        self.cycle_check.clear()
    }

    /// Check new type `id`.
    ///
    /// If type was already encountered in this `Checker` (and thus its `id` is
    /// in `cycle_check`), the decoding has entered a cycle and must be stopped.
    /// If not, type `id` is added into `cycle_check`.
    pub fn check_id<E: ExternalMemory>(&mut self, id: u32) -> Result<(), ParserError<E>> {
        if self.cycle_check.contains(&id) {
            Err(ParserError::CyclicMetadata { id })
        } else {
            self.cycle_check.push(id);
            Ok(())
        }
    }
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

/// Propagating data and collected type information (`Checker` and all non-empty
/// type info).
#[derive(Clone, Debug)]
pub struct Propagated {
    /// Type data that is collected and checked during parsing.
    pub checker: Checker,

    /// Set of [`Info`] collected while resolving the type.
    ///
    /// Only non-empty [`Info`] entries are added.
    pub info: Vec<Info>,
}

impl Propagated {
    /// Initiate new `Propagated` in decoding sequence.
    pub fn new() -> Self {
        Self {
            checker: Checker::new(),
            info: Vec::new(),
        }
    }

    /// Initiate new `Propagated` for signed extensions instance.
    pub fn from_ext_meta(signed_ext_meta: &SignedExtensionMetadata<PortableForm>) -> Self {
        Self {
            checker: Checker {
                specialty_set: SpecialtySet {
                    compact_at: None,
                    hint: Hint::from_ext_meta(signed_ext_meta),
                },
                cycle_check: Vec::new(),
            },
            info: Vec::new(),
        }
    }

    /// Initiate new `Propagated` with known, propagated from above `Checker`.
    pub fn with_checker(checker: Checker) -> Self {
        Self {
            checker,
            info: Vec::new(),
        }
    }

    /// Initiate new `Propagated` with known, propagated from above `Checker`
    /// for an individual [`Field`].
    pub fn for_field<E: ExternalMemory>(
        checker: &Checker,
        field: &Field<PortableForm>,
    ) -> Result<Self, ParserError<E>> {
        Ok(Self {
            checker: Checker::update_for_field(checker, field)?,
            info: Vec::new(),
        })
    }

    /// Initiate new `Propagated` with known, propagated from above `Checker`
    /// for a [`Type`].
    pub fn for_ty<E: ExternalMemory>(
        checker: &Checker,
        ty: &Type<PortableForm>,
        id: u32,
    ) -> Result<Self, ParserError<E>> {
        Ok(Self {
            checker: Checker::update_for_ty(checker, ty, id)?,
            info: Vec::new(),
        })
    }

    /// Get associated `compact_at`
    pub fn compact_at(&self) -> Option<u32> {
        self.checker.specialty_set.compact_at
    }

    /// Check that `compact_at` field in associated [`SpecialtySet`] is not
    /// `Some(_)`, i.e. there was no compact type encountered.
    pub fn reject_compact<E: ExternalMemory>(&self) -> Result<(), ParserError<E>> {
        self.checker.specialty_set.reject_compact::<E>()
    }

    /// Discard previously found [`Hint`].
    pub fn forget_hint(&mut self) {
        self.checker.forget_hint()
    }

    /// Add [`Info`] entry (if non-empty) to `info` set.
    pub fn add_info(&mut self, info_update: &Info) {
        if !info_update.is_empty() {
            self.info.push(info_update.clone())
        }
    }

    /// Add `&[Info]` to `info` set.
    pub fn add_info_slice(&mut self, info_update_slice: &[Info]) {
        self.info.extend_from_slice(info_update_slice)
    }
}

impl Default for Propagated {
    fn default() -> Self {
        Self::new()
    }
}
