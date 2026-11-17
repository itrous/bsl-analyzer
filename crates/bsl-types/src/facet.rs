use std::sync::Arc;

use bsl_metadata::{MdoType, Name};

use crate::kind::{ConfigId, ExprRef, LiteralValue, MetadataKind, Projection, TypeId, TypeOrigin};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct NumberFacet {
    pub precision: Option<u8>,
    pub scale: Option<u8>,
    pub origin: Option<TypeOrigin>,
}

impl NumberFacet {
    pub const fn unsized_() -> Self {
        Self { precision: None, scale: None, origin: None }
    }

    pub const fn with_scale(precision: u8, scale: u8) -> Self {
        Self { precision: Some(precision), scale: Some(scale), origin: None }
    }

    pub const fn with_precision(precision: u8) -> Self {
        Self { precision: Some(precision), scale: None, origin: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct StringFacet {
    pub length: Option<u32>,
    pub fixed: bool,
    pub origin: Option<TypeOrigin>,
}

impl StringFacet {
    pub const fn unsized_() -> Self {
        Self { length: None, fixed: false, origin: None }
    }

    pub const fn with_length(length: u32) -> Self {
        Self { length: Some(length), fixed: false, origin: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DateComponent {
    Date,
    Time,
    DateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DateFacet {
    pub component: DateComponent,
    pub origin: Option<TypeOrigin>,
}

impl DateFacet {
    pub const fn datetime() -> Self {
        Self { component: DateComponent::DateTime, origin: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MetaRefFacet {
    pub kind: MetadataKind,
    pub name: Name,
    pub config_id: ConfigId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MetaObjFacet {
    pub kind: MetadataKind,
    pub name: Name,
    pub config_id: ConfigId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ManagerFacet {
    pub mdo: MdoType,
    pub name: Name,
    pub config_id: ConfigId,
}

impl ManagerFacet {
    pub fn new(mdo: MdoType, name: Name, config_id: ConfigId) -> Self {
        Self { mdo, name, config_id }
    }
}

/// A common module accessed as a value, e.g. `ОбщегоНазначения.ОбщийМодуль("Имя")`.
/// `name` is the canonical (original-case) module name; member access resolves against
/// the module's exported methods.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CommonModuleFacet {
    pub name: Name,
    pub config_id: ConfigId,
}

impl CommonModuleFacet {
    pub fn new(name: Name, config_id: ConfigId) -> Self {
        Self { name, config_id }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FormDataFacet {
    Structure,
    Collection,
    StructureWithCollection,
    Tree,
}

impl FormDataFacet {
    pub fn platform_type_name(&self) -> &'static str {
        match self {
            Self::Structure => "ДанныеФормыСтруктура",
            Self::Collection => "ДанныеФормыКоллекция",
            Self::StructureWithCollection => "ДанныеФормыСтруктураСКоллекцией",
            Self::Tree => "ДанныеФормыДерево",
        }
    }
}

pub use bsl_metadata::FormElementKind as FormElementFacet;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MdoRefFacet {
    pub mdo_type: MdoType,
    pub name: Name,
}

impl MdoRefFacet {
    pub fn new(mdo_type: MdoType, name: Name) -> Self {
        Self { mdo_type, name }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FormBindingFacet {
    pub path: Arc<[Name]>,
    pub target: FormBindingTargetFacet,
}

impl FormBindingFacet {
    pub fn new(path: Arc<[Name]>, target: FormBindingTargetFacet) -> Self {
        Self { path, target }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FormBindingTargetFacet {
    TabularSection { mdo_ref: MdoRefFacet, section: Name },
    Attribute { ty: TypeId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TableSource {
    SdblUnload,
    NewValueTable,
    FormAttribute,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct TableFacet {
    pub projection: Option<Arc<Projection>>,
    pub source: TableSource,
}

impl TableFacet {
    pub fn unprojected() -> Self {
        Self { projection: None, source: TableSource::Unknown }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ArrayFacet {
    pub element: Option<TypeId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MapFacet {
    pub key: Option<TypeId>,
    pub value: Option<TypeId>,
    /// Value types of known string keys, documented as quoted key items; `None` for a map whose
    /// keys are unknown. Display ignores it: a keyed map is still shown as `Соответствие`.
    pub entries: Option<Arc<crate::kind::Projection>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct StructureFacet {
    /// Key names in field order. Kept for back-compat and name-only display; always derivable
    /// from [`Self::fields`] when that is present.
    pub keys: Option<Arc<[Name]>>,
    /// Typed field projection: `None` = untyped structure (keys unknown), `Some` = keys inferred
    /// from a literal `Новый Структура` / `.Вставить` construction (this plan) or a doc-comment.
    pub fields: Option<Arc<Projection>>,
    /// Marker recording where the fields came from (`BslLiteral` / `DocComment`).
    pub origin: Option<TypeOrigin>,
    /// `true` only when the analyzer proved that [`Self::fields`] is the complete, non-empty key
    /// set. Open and keyless structures keep permissive field access.
    pub closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProjectionSource {
    Sdbl,
    FormAttribute,
    ValueTableLiteral,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ProjectionFacet {
    pub projection: Option<Arc<Projection>>,
    pub source: ProjectionSource,
}

impl ProjectionFacet {
    pub fn empty(source: ProjectionSource) -> Self {
        Self { projection: None, source }
    }

    pub fn with(projection: Arc<Projection>, source: ProjectionSource) -> Self {
        Self { projection: Some(projection), source }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct SdblTypeShadowFacet {
    pub display: String,
}

impl SdblTypeShadowFacet {
    pub fn new(display: String) -> Self {
        Self { display }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PlatformObjectFacet {
    pub name: Name,
    /// The model object this value is a description of, when the producer knew it.
    /// It refines the value without changing what the platform type is: the name
    /// above still decides members, methods, display and assignability.
    pub described: Option<DescribedObject>,
}

/// The configuration object a metadata-description value belongs to
/// (`Метаданные.Справочники.Товары` and what hangs off it).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct DescribedObject {
    pub kind: MdoType,
    pub name: Name,
    pub part: DescribedPart,
}

impl DescribedObject {
    pub fn new(kind: MdoType, name: Name, part: DescribedPart) -> Self {
        Self { kind, name, part }
    }
}

/// Which description of the object a value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DescribedPart {
    /// The object's own description (`ОбъектМетаданных: Справочник`).
    Description,
    /// The collection of its attribute descriptions (`….Реквизиты`).
    Attributes,
}

pub use bsl_platform::ParamPassing;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArgArity {
    Fixed(u16),
    Variadic,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct ParamSpec {
    pub name: Name,
    pub ty: TypeId,
    pub passing: ParamPassing,
    pub variadic: bool,
}

impl ParamSpec {
    pub fn new(name: Name, ty: TypeId, passing: ParamPassing, variadic: bool) -> Self {
        Self { name, ty, passing, variadic }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DefaultValue {
    Literal(LiteralValue),
    NamedConstant(Name),
    DeferredExpr(ExprRef),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FunctionOrigin {
    UserDefined,
    PlatformGlobal,
    Closure,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FunctionFacet {
    pub params: Arc<[ParamSpec]>,
    pub defaults: Arc<[Option<DefaultValue>]>,
    pub min_args: u16,
    pub max_args: ArgArity,
    pub returns: TypeId,
    pub origin: FunctionOrigin,
}

impl FunctionFacet {
    pub fn new(
        params: Arc<[ParamSpec]>,
        defaults: Arc<[Option<DefaultValue>]>,
        min_args: u16,
        max_args: ArgArity,
        returns: TypeId,
        origin: FunctionOrigin,
    ) -> Self {
        Self { params, defaults, min_args, max_args, returns, origin }
    }
}

pub use crate::kind::ProjectionFieldSource;
