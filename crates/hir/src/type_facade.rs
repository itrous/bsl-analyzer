use bsl_metadata::{Form, FormElement, MdoType};
use bsl_platform::{PlatformData, PlatformMethod};
use bsl_types::builders::Builders;
use bsl_types::display::{display_name as kernel_display, Locale as KernelLocale, PlainDisplayCtx};
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{Projection, TypeId, TypeKind};
use hir_def::configs::ConfigsDatabase;
use hir_def::ty::MetadataKind;
use hir_def::Name;
use hir_ty::lower::type_string::{lower_param_type_string_typeid, lower_return_type_string_typeid};
use hir_ty::method_lookup::platform_type_key_id;
use hir_ty::{
    enumerate_fields, is_assignable, is_ref_ty, lookup_field, lookup_manager_field, lookup_method,
    platform_methods_for_manager, platform_methods_for_metadata_kind, FieldInfo, FieldOrigin,
};
use std::sync::Arc;
use stdx::case::CaseExt;
use vfs::FileId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub name: Name,
    pub english_name: Name,
    pub return_ty: Option<TypeId>,
    pub params: Vec<MethodParam>,
    pub env: Option<hir_def::execution_env::EnvFlags>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodParam {
    pub name: Name,
    pub ty: Option<TypeId>,
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformNameKind {
    GlobalFunction,
    Type,
    Method,
    Property,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformNameEntry<'a> {
    pub kind: PlatformNameKind,
    pub name: &'a str,
    pub english_name: &'a str,
    pub owner: Option<&'a str>,
}

/// The platform name surface exposed to IDE consumers through the HIR facade.
pub fn platform_name_entries() -> Vec<PlatformNameEntry<'static>> {
    let platform = PlatformData::instance();
    let mut entries = Vec::with_capacity(
        platform.all_global_functions().len()
            + platform.all_types().len()
            + platform.all_methods().len()
            + platform.all_properties().len(),
    );

    entries.extend(platform.all_global_functions().iter().map(|function| PlatformNameEntry {
        kind: PlatformNameKind::GlobalFunction,
        name: &function.name,
        english_name: &function.english_name,
        owner: None,
    }));
    entries.extend(platform.all_types().iter().map(|ty| PlatformNameEntry {
        kind: PlatformNameKind::Type,
        name: &ty.name,
        english_name: &ty.english_name,
        owner: None,
    }));
    entries.extend(platform.all_methods().iter().map(|method| PlatformNameEntry {
        kind: PlatformNameKind::Method,
        name: &method.name,
        english_name: &method.english_name,
        owner: platform_owner_display(platform, &method.type_name),
    }));
    entries.extend(platform.all_properties().iter().map(|property| PlatformNameEntry {
        kind: PlatformNameKind::Property,
        name: &property.name,
        english_name: &property.english_name,
        owner: platform_owner_display(platform, &property.type_name),
    }));
    entries
}

fn platform_owner_display<'a>(platform: &'a PlatformData, type_name: &'a str) -> Option<&'a str> {
    if type_name.is_empty() {
        None
    } else {
        Some(platform.get_type(type_name).map_or(type_name, |ty| ty.name.as_str()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HirFieldOrigin {
    StandardAttribute,
    UserAttribute,
    FormAttribute,
    MainFormAttribute,
    TabularSection,
    TabularSectionRowColumn,
    RegisterDimension,
    RegisterResource,
    RegisterAttribute,
    MetadataReference,
    PlatformProperty,
}

impl From<FieldOrigin> for HirFieldOrigin {
    fn from(o: FieldOrigin) -> Self {
        match o {
            FieldOrigin::StandardAttribute => Self::StandardAttribute,
            FieldOrigin::UserAttribute => Self::UserAttribute,
            FieldOrigin::FormAttribute => Self::FormAttribute,
            FieldOrigin::MainFormAttribute => Self::MainFormAttribute,
            FieldOrigin::TabularSection => Self::TabularSection,
            FieldOrigin::TabularSectionRowColumn => Self::TabularSectionRowColumn,
            FieldOrigin::RegisterDimension => Self::RegisterDimension,
            FieldOrigin::RegisterResource => Self::RegisterResource,
            FieldOrigin::RegisterAttribute => Self::RegisterAttribute,
            FieldOrigin::MetadataReference => Self::MetadataReference,
            FieldOrigin::PlatformProperty { .. } => Self::PlatformProperty,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: Name,
    pub english_name: Name,
    pub ty: TypeId,
    pub value_ty: Option<TypeId>,
    pub is_readonly: bool,
    pub origin: HirFieldOrigin,
    pub env: Option<hir_def::execution_env::EnvFlags>,
}

#[derive(Debug)]
pub struct Type<'db, DB> {
    db: &'db DB,
    file_id: FileId,
    id: TypeId,
}

impl<'db, DB: ConfigsDatabase + TypeKernelDb> Type<'db, DB> {
    pub fn from_id(db: &'db DB, file_id: FileId, id: TypeId) -> Self {
        Self { db, file_id, id }
    }

    pub fn id(&self) -> TypeId {
        self.id
    }

    pub fn kind(&self) -> &TypeKind {
        self.db.lookup_type(self.id)
    }

    pub fn display_name(&self, locale: base_db::Locale) -> String {
        kernel_type_label(self.db, self.id, locale, false)
    }

    pub fn canonical_name(&self) -> String {
        kernel_type_label(self.db, self.id, base_db::Locale::En, false)
    }

    pub fn is_ref_type(&self) -> bool {
        is_ref_ty(self.db, self.id)
    }

    pub fn is_assignable_to(&self, other: &Self) -> bool {
        is_assignable(self.db, self.id, other.id)
    }

    pub fn manager(&self) -> Option<Self> {
        let (kind, name, config_id) = match self.kind() {
            TypeKind::MetadataRef(facet) => {
                (facet.kind, facet.name.clone(), facet.config_id.clone())
            }
            TypeKind::MetadataObject(facet) => {
                (facet.kind, facet.name.clone(), facet.config_id.clone())
            }
            _ => return None,
        };
        let mdo_type = match kind {
            MetadataKind::CatalogRef | MetadataKind::CatalogObject => MdoType::Catalog,
            MetadataKind::DocumentRef | MetadataKind::DocumentObject => MdoType::Document,
            MetadataKind::EnumRef => MdoType::Enum,
            MetadataKind::TaskRef | MetadataKind::TaskObject => MdoType::Task,
            MetadataKind::BusinessProcessRef | MetadataKind::BusinessProcessObject => {
                MdoType::BusinessProcess
            }
            MetadataKind::DataProcessorObject => MdoType::DataProcessor,
            MetadataKind::ReportObject => MdoType::Report,
            MetadataKind::ExternalDataProcessorObject => MdoType::ExternalDataProcessor,
            MetadataKind::ExternalReportObject => MdoType::ExternalReport,
            MetadataKind::ExchangePlanRef | MetadataKind::ExchangePlanObject => {
                MdoType::ExchangePlan
            }
            MetadataKind::ChartOfAccountsRef | MetadataKind::ChartOfAccountsObject => {
                MdoType::ChartOfAccounts
            }
            MetadataKind::ChartOfCharacteristicTypesRef
            | MetadataKind::ChartOfCharacteristicTypesObject => MdoType::ChartOfCharacteristicTypes,
            MetadataKind::ChartOfCalculationTypesRef
            | MetadataKind::ChartOfCalculationTypesObject => MdoType::ChartOfCalculationTypes,
            MetadataKind::InformationRegisterRef => MdoType::InformationRegister,
            MetadataKind::AccumulationRegisterRef => MdoType::AccumulationRegister,
            MetadataKind::AccountingRegisterRef => MdoType::AccountingRegister,
            MetadataKind::CalculationRegisterRef => MdoType::CalculationRegister,
            MetadataKind::InformationRegisterRecordManager
            | MetadataKind::InformationRegisterRecordSet
            | MetadataKind::InformationRegisterRecord
            | MetadataKind::AccumulationRegisterRecordSet
            | MetadataKind::AccumulationRegisterRecord
            | MetadataKind::AccountingRegisterRecordSet
            | MetadataKind::AccountingRegisterRecord
            | MetadataKind::CalculationRegisterRecordSet
            | MetadataKind::CalculationRegisterRecord
            | MetadataKind::RegisterDimension { .. }
            | MetadataKind::RegisterResource { .. }
            | MetadataKind::RegisterAttribute { .. }
            | MetadataKind::RegisterFilter { .. }
            | MetadataKind::TabularSection { .. }
            | MetadataKind::TabularSectionRow { .. } => return None,
        };

        let id = self.db.object_manager_with_config(mdo_type, name, config_id);
        Some(Self::from_id(self.db, self.file_id, id))
    }

    pub fn method_return_type(&self, method_name: &Name) -> Self {
        let id = lookup_method(self.db, self.id, method_name)
            .map(|info| info.return_ty)
            .unwrap_or_else(|| self.db.unknown());
        Self::from_id(self.db, self.file_id, id)
    }

    pub fn field_type(&self, field_name: &Name) -> Self {
        let obj_resolver = hir_ty::DbObjectResolver::new(self.db, self.file_id);
        let id = lookup_field(self.db, &obj_resolver, self.id, field_name)
            .map(|info| info.ty)
            .or_else(|| {
                lookup_manager_field(self.db, &obj_resolver, self.id, field_name)
                    .map(|info| info.ty)
            })
            .unwrap_or_else(|| self.db.unknown());
        Self::from_id(self.db, self.file_id, id)
    }

    pub fn has_field(&self, field_name: &Name) -> bool {
        let obj_resolver = hir_ty::DbObjectResolver::new(self.db, self.file_id);
        lookup_field(self.db, &obj_resolver, self.id, field_name).is_some()
            || lookup_manager_field(self.db, &obj_resolver, self.id, field_name).is_some()
    }

    pub fn methods(&self) -> Vec<Method> {
        if let TypeKind::FormControl { kind, .. } = self.kind() {
            let chain = hir_def::ty::form_control_platform_type_chain(*kind);
            let mut methods: Vec<Method> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for type_name in chain.iter().rev() {
                for m in PlatformData::instance().get_type_methods(type_name) {
                    let dto = method_dto_from_platform(self.db, m);
                    if seen.insert(dto.name.as_str().fold_lower()) {
                        methods.push(dto);
                    }
                }
            }
            return methods;
        }

        let metadata_kind = match self.kind() {
            TypeKind::MetadataRef(facet) => Some(facet.kind),
            TypeKind::MetadataObject(facet) => Some(facet.kind),
            _ => None,
        };
        if let Some(kind) = metadata_kind {
            return platform_methods_for_metadata_kind(kind)
                .iter()
                .map(|method| method_dto_from_platform(self.db, method))
                .collect();
        }
        if let TypeKind::ObjectManager(facet) = self.kind() {
            return platform_methods_for_manager(facet.mdo)
                .iter()
                .map(|method| method_dto_from_platform(self.db, method))
                .collect();
        }

        let Some(type_key) = platform_type_key_id(self.db, self.id) else {
            return Vec::new();
        };
        PlatformData::instance()
            .get_type_methods(&type_key)
            .into_iter()
            .map(|m| method_dto_from_platform(self.db, m))
            .collect()
    }

    pub fn fields(&self) -> Vec<Field> {
        let obj_resolver = hir_ty::DbObjectResolver::new(self.db, self.file_id);
        enumerate_fields(self.db, &obj_resolver, self.id)
            .into_iter()
            .map(|info| Field {
                env: match info.origin {
                    FieldOrigin::PlatformProperty { env } => Some(env),
                    _ => None,
                },
                name: info.name.clone(),
                english_name: info.name_en.clone().unwrap_or_else(|| info.name.clone()),
                ty: info.ty,
                value_ty: info.value_ty,
                is_readonly: info.is_readonly,
                origin: HirFieldOrigin::from(info.origin),
            })
            .collect()
    }

    pub fn is_query_projection(&self) -> bool {
        self.projection().is_some()
    }

    fn projection(&self) -> Option<Arc<Projection>> {
        match self.kind() {
            TypeKind::QueryResultSelection(facet) => facet.projection.clone(),
            TypeKind::ValueTable(facet) | TypeKind::ValueTableRow(facet) => {
                facet.projection.clone()
            }
            _ => None,
        }
    }

    pub fn projection_fields(&self) -> Option<Vec<(Name, TypeId)>> {
        let p = self.projection()?;
        Some(p.fields.iter().map(|f| (Name::new(f.name.as_str()), f.ty)).collect())
    }

    pub fn projection_field_displays(&self) -> Option<Vec<String>> {
        let p = self.projection()?;
        p.raw_sdbl_types.as_ref().map(|shadows| shadows.iter().map(|s| s.display.clone()).collect())
    }
}

pub fn root_metadata_object_type(
    db: &dyn TypeKernelDb,
    mdo_type: MdoType,
    name: &str,
) -> Option<TypeId> {
    use bsl_types::testing::RootConfigCtx;

    let kind = MetadataKind::object_kind_for(mdo_type)?;
    Some(db.metadata_object(kind, name.to_string(), &RootConfigCtx))
}

pub fn root_metadata_ref_type(
    db: &dyn TypeKernelDb,
    mdo_type: MdoType,
    name: &str,
) -> Option<TypeId> {
    use bsl_types::testing::RootConfigCtx;

    let kind = MetadataKind::ref_kind_for(mdo_type)?;
    Some(db.metadata_ref(kind, name.to_string(), &RootConfigCtx))
}

pub fn root_object_manager_type(
    db: &dyn TypeKernelDb,
    mdo_type: MdoType,
    name: &str,
) -> Option<TypeId> {
    use bsl_types::testing::RootConfigCtx;

    mdo_type.manager_type_prefix()?;
    Some(db.object_manager(mdo_type, name.to_string(), &RootConfigCtx))
}

pub fn kernel_type_label(
    db: &dyn TypeKernelDb,
    id: TypeId,
    locale: base_db::Locale,
    precision: bool,
) -> String {
    let kernel_locale = match locale {
        base_db::Locale::Ru => KernelLocale::Ru,
        base_db::Locale::En => KernelLocale::En,
    };
    let ctx = PlainDisplayCtx { locale: kernel_locale, precision_visible: precision };
    kernel_display(db.lookup_type(id), &ctx, db)
}

fn field_from_info(info: FieldInfo) -> Field {
    Field {
        env: match info.origin {
            FieldOrigin::PlatformProperty { env } => Some(env),
            _ => None,
        },
        name: info.name.clone(),
        english_name: info.name_en.unwrap_or_else(|| info.name.clone()),
        ty: info.ty,
        value_ty: info.value_ty,
        is_readonly: info.is_readonly,
        origin: HirFieldOrigin::from(info.origin),
    }
}

pub fn module_implicit_fields<DB: hir_ty::db::HirDatabase>(db: &DB, file_id: FileId) -> Vec<Field> {
    hir_ty::module_implicit_fields(db, file_id).into_iter().map(field_from_info).collect()
}

pub fn form_element_type<DB: hir_ty::db::HirDatabase>(
    db: &DB,
    file_id: FileId,
    form: &Form,
    element: &FormElement,
) -> TypeId {
    hir_ty::lower_form_element_for_file(db, file_id, form, element)
}

/// Lowercased ru/en names of the module's implicit context fields, for
/// consumers that only need name membership (e.g. unused-variable analysis).
pub fn module_implicit_field_names(
    db: &dyn hir_ty::db::HirDatabase,
    file_id: FileId,
) -> Vec<String> {
    hir_ty::module_implicit_fields(db, file_id)
        .into_iter()
        .flat_map(|info| {
            let mut names = vec![info.name.as_str().fold_lower()];
            if let Some(en) = &info.name_en {
                names.push(en.as_str().fold_lower());
            }
            names
        })
        .collect()
}

fn method_dto_from_platform(db: &dyn TypeKernelDb, method: &PlatformMethod) -> Method {
    let params = method
        .parameters
        .iter()
        .map(|param| MethodParam {
            name: Name::new(param.name.as_str()),
            ty: param.param_type.as_ref().map(|ty| lower_param_type_string_typeid(db, ty)),
            optional: param.is_optional,
        })
        .collect();
    // Имена берутся у каталога, а не у сырых полей: у шаблонных записей менеджера `name`
    // усечено до `&lt;Имя`, и карточка показала бы двадцать два одинаковых члена с этим
    // именем вместо `СоздатьЭлемент`, `СоздатьГруппу` и остальных.
    let (russian, english) = bsl_platform::method_display_names(method);
    Method {
        name: Name::new(russian.as_str()),
        english_name: fallback_name(english.as_str(), russian.as_str()),
        return_ty: method.return_type.as_ref().map(|ret| lower_return_type_string_typeid(db, ret)),
        params,
        env: Some(hir_def::execution_env::EnvFlags::from_platform_context(method.context.as_ref())),
    }
}

pub fn execution_environment_at<DB: hir_ty::db::HirDatabase>(
    db: &DB,
    file_id: FileId,
    offset: syntax::TextSize,
) -> hir_def::execution_env::EnvFlags {
    use hir_def::execution_env::{self, EnvFlags};

    let options = db.env_options();
    let metadata = db.module_metadata(hir_def::ModuleId::new(file_id));
    let item_tree = db.item_tree(file_id);
    let method_at_cursor = crate::bare_root::method_item_at(&item_tree, offset);
    if let Some(method) = method_at_cursor {
        let annotations = method.annotations();
        if annotations.iter().any(|annotation| {
            matches!(
                annotation.kind,
                hir_def::item_tree::AnnotationKind::Before
                    | hir_def::item_tree::AnnotationKind::After
                    | hir_def::item_tree::AnnotationKind::Instead
                    | hir_def::item_tree::AnnotationKind::ChangeAndValidate
            )
        }) {
            return EnvFlags::EMPTY;
        }
    }

    let mut environment = match method_at_cursor {
        Some(method) => execution_env::method_env(&item_tree, method.key(), &metadata, &options),
        None => execution_env::module_code_env(&metadata, &options),
    };
    if !environment.is_empty() {
        let conditionals = db.conditional_tree(file_id);
        if !conditionals.is_empty() {
            environment = environment & execution_env::conditional_env_at(&conditionals, offset);
        }
    }
    environment
}

fn fallback_name(name: &str, fallback: &str) -> Name {
    if name.is_empty() {
        Name::new(fallback)
    } else {
        Name::new(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_config::ConfigId;
    use bsl_types::facet::{
        ArgArity, FunctionFacet, FunctionOrigin, MdoRefFacet, ParamPassing, ParamSpec,
    };
    use bsl_types::testing::RootConfigCtx;
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn empty_db() -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, "");
        (db, file_id)
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn platform_name_entries_cover_every_kind_and_localize_owners() {
        let entries = platform_name_entries();
        for kind in [
            PlatformNameKind::GlobalFunction,
            PlatformNameKind::Type,
            PlatformNameKind::Method,
            PlatformNameKind::Property,
        ] {
            assert!(entries.iter().any(|entry| entry.kind == kind), "missing {kind:?}");
        }
        assert!(entries.iter().any(|entry| {
            entry.kind == PlatformNameKind::Method
                && entry.owner.is_some_and(|owner| !owner.is_ascii())
        }));
    }

    fn designer_fixture_path() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
    }

    fn db_with_configuration(config_path: PathBuf) -> (RootDatabaseImpl, FileId) {
        let (mut db, file_id) = empty_db();
        db.set_all_config_paths(vec![(None, config_path)]);
        (db, file_id)
    }

    fn db_at_with_configuration(
        module_path: PathBuf,
        config_path: PathBuf,
    ) -> (RootDatabaseImpl, FileId) {
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let mut file_set = vfs::FileSet::default();
        file_set.insert(file_id, vfs::VfsPath::new(module_path.to_string_lossy().to_string()));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, "");
        db.set_all_config_paths(vec![(None, config_path)]);
        (db, file_id)
    }

    fn copy_dir_all(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).expect("create temp fixture dir");
        for entry in fs::read_dir(src).expect("read fixture dir") {
            let entry = entry.expect("read fixture entry");
            let ty = entry.file_type().expect("fixture entry type");
            let dst_path = dst.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_all(&entry.path(), &dst_path);
            } else {
                fs::copy(entry.path(), dst_path).expect("copy fixture file");
            }
        }
    }

    struct TempFixture {
        path: PathBuf,
    }

    impl TempFixture {
        fn duplicated_field() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("bsl-analyzer-type-facade-{}-{unique}", std::process::id()));
            copy_dir_all(&designer_fixture_path(), &path);

            let catalog_path = path.join("Catalogs/Справочник1.xml");
            let xml = fs::read_to_string(&catalog_path).expect("read copied catalog xml");
            let xml = xml.replacen("<Name>ТабличнаяЧасть1</Name>", "<Name>Реквизит2</Name>", 1);
            fs::write(&catalog_path, xml).expect("write copied catalog xml");

            Self { path }
        }

        fn information_register_with_resource() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("bsl-analyzer-type-facade-{}-{unique}", std::process::id()));
            copy_dir_all(&designer_fixture_path(), &path);

            let register_path = path.join("InformationRegisters/РегистрСведений1.xml");
            let xml = fs::read_to_string(&register_path).expect("read copied register xml");
            let resource = r#"
			<Resource uuid="11111111-2222-3333-4444-555555555555">
				<Properties>
					<Name>Количество</Name>
					<Type>
						<v8:Type>xs:decimal</v8:Type>
						<v8:NumberQualifiers>
							<v8:Digits>15</v8:Digits>
							<v8:FractionDigits>3</v8:FractionDigits>
						</v8:NumberQualifiers>
					</Type>
				</Properties>
			</Resource>"#;
            let xml =
                xml.replacen("</ChildObjects>", &format!("{resource}\n\t\t</ChildObjects>"), 1);
            fs::write(&register_path, xml).expect("write copied register xml");

            Self { path }
        }

        fn path(&self) -> PathBuf {
            self.path.clone()
        }
    }

    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn display_name_matches_ty() {
        use base_db::Locale;
        let (db, file_id) = empty_db();
        let t = t(&db, file_id, db.number(None, None));
        assert_eq!(t.display_name(Locale::En), "Number");
        assert_eq!(t.display_name(Locale::Ru), "Число");
        assert_eq!(t.canonical_name(), "Number");
    }

    #[test]
    fn is_ref_type_true_for_metadata_refs() {
        let (db, file_id) = empty_db();
        let catalog = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogRef, "X"));
        assert!(catalog.is_ref_type());

        let catalog_obj = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogObject, "X"));
        assert!(!catalog_obj.is_ref_type(), "CatalogObject is not a ref type (it is an object)");

        let row = t(
            &db,
            file_id,
            metadata_ref(
                &db,
                MetadataKind::TabularSectionRow { parent: MdoType::Document },
                "X.Section",
            ),
        );
        assert!(!row.is_ref_type());

        assert!(!t(&db, file_id, db.number(None, None)).is_ref_type());
    }

    #[test]
    fn manager_from_ref_types() {
        let (db, file_id) = empty_db();
        let cat = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogRef, "Номенклатура"));
        let manager = cat.manager().expect("CatalogRef has a manager form");
        match db.lookup_type(manager.id()) {
            TypeKind::ObjectManager(facet) => {
                assert_eq!(facet.mdo, MdoType::Catalog);
                assert_eq!(facet.name.as_str(), "Номенклатура");
            }
            other => panic!("expected ObjectManager, got {other:?}"),
        }
    }

    #[test]
    fn manager_none_for_non_refs() {
        let (db, file_id) = empty_db();
        assert!(t(&db, file_id, db.number(None, None)).manager().is_none());
        assert!(t(&db, file_id, db.array(None)).manager().is_none());
    }

    #[test]
    fn manager_from_register_ref_types() {
        let (db, file_id) = empty_db();
        let reg = t(&db, file_id, metadata_ref(&db, MetadataKind::AccumulationRegisterRef, "X"));
        let manager = reg.manager().expect("register ref has a manager form");
        match db.lookup_type(manager.id()) {
            TypeKind::ObjectManager(facet) => {
                assert_eq!(facet.mdo, MdoType::AccumulationRegister);
                assert_eq!(facet.name.as_str(), "X");
            }
            other => panic!("expected ObjectManager, got {other:?}"),
        }
    }

    #[test]
    fn manager_from_metadata_object_receiver() {
        use bsl_types::testing::RootConfigCtx;
        let (db, file_id) = empty_db();
        let id = db.metadata_object(
            MetadataKind::CatalogObject,
            "Номенклатура".to_string(),
            &RootConfigCtx,
        );
        let manager = Type::from_id(&db, file_id, id)
            .manager()
            .expect("MetadataObject receiver has a manager form");
        match db.lookup_type(manager.id()) {
            TypeKind::ObjectManager(facet) => {
                assert_eq!(facet.mdo, MdoType::Catalog);
                assert_eq!(facet.name.as_str(), "Номенклатура");
            }
            other => panic!("expected ObjectManager, got {other:?}"),
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn method_return_type_on_array_is_known() {
        let (db, file_id) = empty_db();
        let arr = t(&db, file_id, db.array(None));
        let ret = arr.method_return_type(&Name::new("Добавить"));
        assert_eq!(ret.id(), db.undefined());
    }

    #[test]
    fn method_return_type_unknown_for_missing() {
        let (db, file_id) = empty_db();
        let arr = t(&db, file_id, db.array(None));
        let ret = arr.method_return_type(&Name::new("НеСуществует"));
        assert_eq!(ret.id(), db.unknown());
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn methods_lists_platform_methods_for_array() {
        let (db, file_id) = empty_db();
        let arr = t(&db, file_id, db.array(None));
        let methods = arr.methods();
        assert!(!methods.is_empty(), "Array must expose at least one platform method");
        assert!(
            methods.iter().any(|m| m.name.as_str() == "Добавить"),
            "Array methods must include Добавить — got {:?}",
            methods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn methods_list_prefixed_platform_methods_for_metadata_refs() {
        let (db, file_id) = empty_db();
        let cat = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogRef, "X"));
        assert!(!cat.methods().is_empty());
        assert!(t(&db, file_id, db.number(None, None)).methods().is_empty());
    }

    #[test]
    fn fields_empty_without_configuration() {
        let (db, file_id) = empty_db();
        let cat = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogRef, "X"));
        assert!(cat.fields().is_empty());
    }

    #[test]
    fn fields_include_custom_attributes_from_configuration() {
        let (db, file_id) = db_with_configuration(designer_fixture_path());
        let cat = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogRef, "Справочник1"));

        let fields = cat.fields();
        let attr = fields
            .iter()
            .find(|field| field.name == Name::new("Реквизит2"))
            .expect("custom attribute must be present");
        assert_eq!(attr.english_name, Name::new("Реквизит2"));
        assert_eq!(attr.ty, db.number(None, None));
    }

    #[test]
    fn fields_on_metadata_object_receiver_surface_attributes() {
        use bsl_types::testing::RootConfigCtx;
        let (db, file_id) = db_with_configuration(designer_fixture_path());
        let id = db.metadata_object(
            MetadataKind::CatalogObject,
            "Справочник1".to_string(),
            &RootConfigCtx,
        );
        let fields = Type::from_id(&db, file_id, id).fields();
        assert!(
            fields.iter().any(|f| f.name == Name::new("Реквизит2")),
            "MetadataObject receiver must surface MDO custom attributes; got {:?}",
            fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn fields_include_tabular_sections_from_configuration() {
        let (db, file_id) = db_with_configuration(designer_fixture_path());
        let cat = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogRef, "Справочник1"));

        let fields = cat.fields();
        let section = fields
            .iter()
            .find(|field| field.name == Name::new("ТабличнаяЧасть1"))
            .expect("tabular section must be present");
        assert_eq!(section.english_name, Name::new("ТабличнаяЧасть1"));
        assert_eq!(
            section.ty,
            metadata_ref(
                &db,
                MetadataKind::TabularSection { parent: MdoType::Catalog },
                "Справочник1.ТабличнаяЧасть1",
            )
        );
    }

    #[test]
    fn fields_include_register_parts_from_configuration() {
        let (db, file_id) = db_with_configuration(designer_fixture_path());
        let reg = t(
            &db,
            file_id,
            metadata_ref(&db, MetadataKind::InformationRegisterRef, "РегистрСведений1"),
        );

        let fields = reg.fields();
        let dim = fields
            .iter()
            .find(|field| field.name == Name::new("Справочник1"))
            .expect("register dimension must appear in .fields()");
        assert_eq!(
            dim.ty,
            metadata_ref(&db, MetadataKind::CatalogRef, "Справочник1"),
            "typed dimension must lower through TyLoweringContext, not fall back to symbolic",
        );
    }

    #[test]
    fn module_implicit_fields_object_module_yields_mdo_attributes() {
        let config_path = designer_fixture_path();
        let module_path = config_path.join("DataProcessors/ТестоваяОбработка/Ext/ObjectModule.bsl");
        let (db, file_id) = db_at_with_configuration(module_path, config_path);

        let fields = module_implicit_fields(&db, file_id);
        let attr = fields
            .iter()
            .find(|field| field.name == Name::new("АдресСайта"))
            .expect("object module must expose owner MDO attributes as bare identifiers");

        assert_eq!(attr.ty, db.string(None, false));
    }

    #[test]
    fn module_implicit_fields_record_set_module_yields_dimensions_and_resources() {
        let fixture = TempFixture::information_register_with_resource();
        let config_path = fixture.path();
        let module_path =
            config_path.join("InformationRegisters/РегистрСведений1/Ext/RecordSetModule.bsl");
        let (db, file_id) = db_at_with_configuration(module_path, config_path);

        let fields = module_implicit_fields(&db, file_id);

        assert!(
            fields.iter().any(|field| {
                field.name == Name::new("Справочник1")
                    && field.origin == HirFieldOrigin::RegisterDimension
            }),
            "record-set module must expose register dimensions",
        );
        assert!(
            fields.iter().any(|field| {
                field.name == Name::new("Количество")
                    && field.origin == HirFieldOrigin::RegisterResource
            }),
            "record-set module must expose register resources",
        );
    }

    #[test]
    fn module_implicit_fields_managed_form_yields_form_attributes_with_origin() {
        let config_path = designer_fixture_path();
        let main_module_path =
            config_path.join("DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl");
        let regular_module_path =
            config_path.join("Catalogs/рдт_Рецептура/Forms/ФормаЭлемента/Ext/Form/Module.bsl");

        let (db, file_id) = db_at_with_configuration(main_module_path, config_path.clone());
        let fields = module_implicit_fields(&db, file_id);
        let main = fields
            .iter()
            .find(|field| field.name == Name::new("Объект"))
            .expect("managed form must expose main form attribute");
        assert_eq!(main.origin, HirFieldOrigin::MainFormAttribute);

        let (db, file_id) = db_at_with_configuration(regular_module_path, config_path);
        let fields = module_implicit_fields(&db, file_id);
        let plain = fields
            .iter()
            .find(|field| field.name == Name::new("Пересчитать"))
            .expect("managed form must expose regular form attribute");
        assert_eq!(plain.origin, HirFieldOrigin::FormAttribute);
    }

    #[test]
    fn module_implicit_fields_manager_module_yields_empty() {
        let config_path = designer_fixture_path();
        let module_path = config_path.join("Catalogs/Справочник1/Ext/ManagerModule.bsl");
        let (db, file_id) = db_at_with_configuration(module_path, config_path);

        assert!(module_implicit_fields(&db, file_id).is_empty());
    }

    fn t(db: &RootDatabaseImpl, file_id: FileId, id: TypeId) -> Type<'_, RootDatabaseImpl> {
        Type::from_id(db, file_id, id)
    }

    fn metadata_ref(db: &RootDatabaseImpl, kind: MetadataKind, name: &str) -> TypeId {
        db.metadata_ref(kind, name.to_string(), &RootConfigCtx)
    }

    fn fixed_function(db: &RootDatabaseImpl, params: Vec<TypeId>, returns: TypeId) -> TypeId {
        let params: Arc<[ParamSpec]> = params
            .into_iter()
            .enumerate()
            .map(|(idx, ty)| ParamSpec::new(format!("p{idx}"), ty, ParamPassing::ByRef, false))
            .collect();
        let arity = u16::try_from(params.len()).expect("test function arity fits u16");
        let defaults = vec![None; params.len()].into();
        db.function(FunctionFacet::new(
            params,
            defaults,
            arity,
            ArgArity::Fixed(arity),
            returns,
            FunctionOrigin::Unknown,
        ))
    }

    #[test]
    fn is_assignable_reflexive_on_primitives() {
        let (db, file_id) = empty_db();
        assert!(t(&db, file_id, db.number(None, None)).is_assignable_to(&t(
            &db,
            file_id,
            db.number(None, None)
        )));
        assert!(t(&db, file_id, db.string(None, false)).is_assignable_to(&t(
            &db,
            file_id,
            db.string(None, false)
        )));
        assert!(t(&db, file_id, db.boolean()).is_assignable_to(&t(&db, file_id, db.boolean())));
        assert!(!t(&db, file_id, db.number(None, None)).is_assignable_to(&t(
            &db,
            file_id,
            db.string(None, false)
        )));
    }

    #[test]
    fn is_assignable_reflexive_on_metadata_ref() {
        let (db, file_id) = empty_db();
        let cat_x = metadata_ref(&db, MetadataKind::CatalogRef, "X");
        let cat_y = metadata_ref(&db, MetadataKind::CatalogRef, "Y");
        assert!(t(&db, file_id, cat_x).is_assignable_to(&t(&db, file_id, cat_x)));
        assert!(!t(&db, file_id, cat_x).is_assignable_to(&t(&db, file_id, cat_y)));
    }

    #[test]
    fn is_assignable_unknown_is_top_and_bottom() {
        let (db, file_id) = empty_db();
        assert!(t(&db, file_id, db.number(None, None)).is_assignable_to(&t(
            &db,
            file_id,
            db.unknown()
        )));
        assert!(t(&db, file_id, db.unknown()).is_assignable_to(&t(
            &db,
            file_id,
            db.number(None, None)
        )));
        assert!(t(&db, file_id, db.unknown()).is_assignable_to(&t(&db, file_id, db.unknown())));
    }

    #[test]
    fn is_assignable_null_to_ref_types_only() {
        let (db, file_id) = empty_db();
        let null = t(&db, file_id, db.null());
        for kind in [
            MetadataKind::CatalogRef,
            MetadataKind::DocumentRef,
            MetadataKind::EnumRef,
            MetadataKind::TaskRef,
            MetadataKind::BusinessProcessRef,
            MetadataKind::ExchangePlanRef,
            MetadataKind::ChartOfAccountsRef,
            MetadataKind::InformationRegisterRef,
            MetadataKind::AccumulationRegisterRef,
            MetadataKind::AccountingRegisterRef,
            MetadataKind::CalculationRegisterRef,
        ] {
            let target = t(&db, file_id, metadata_ref(&db, kind, "X"));
            assert!(null.is_assignable_to(&target), "Null should be assignable to {kind:?}");
        }
        let cat_obj = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogObject, "X"));
        assert!(!null.is_assignable_to(&cat_obj));
        assert!(!null.is_assignable_to(&t(&db, file_id, db.number(None, None))));
    }

    #[test]
    fn is_assignable_element_to_union_right() {
        let (db, file_id) = empty_db();
        let number_or_string = db.union(vec![db.number(None, None), db.string(None, false)]);
        assert!(t(&db, file_id, db.number(None, None)).is_assignable_to(&t(
            &db,
            file_id,
            number_or_string
        )));
        assert!(t(&db, file_id, db.string(None, false)).is_assignable_to(&t(
            &db,
            file_id,
            number_or_string
        )));
        assert!(!t(&db, file_id, db.boolean()).is_assignable_to(&t(
            &db,
            file_id,
            number_or_string
        )));
    }

    #[test]
    fn is_assignable_union_left_distributes() {
        let (db, file_id) = empty_db();
        let ns = db.union(vec![db.number(None, None), db.string(None, false)]);
        assert!(!t(&db, file_id, ns).is_assignable_to(&t(&db, file_id, db.number(None, None))));
        assert!(!t(&db, file_id, ns).is_assignable_to(&t(&db, file_id, db.string(None, false))));

        let nsb = db.union(vec![db.number(None, None), db.string(None, false), db.boolean()]);
        assert!(t(&db, file_id, ns).is_assignable_to(&t(&db, file_id, ns)));
        assert!(t(&db, file_id, ns).is_assignable_to(&t(&db, file_id, nsb)));
    }

    #[test]
    fn is_assignable_this_object_coerces_to_metadata_ref() {
        let (db, file_id) = empty_db();
        let this_cat = db.mk_this_object(
            ConfigId::Root,
            MdoRefFacet::new(MdoType::Catalog, "Товары".to_string()),
        );
        let cat_object = metadata_ref(&db, MetadataKind::CatalogObject, "Товары");
        assert!(t(&db, file_id, this_cat).is_assignable_to(&t(&db, file_id, cat_object)));
        assert!(
            !t(&db, file_id, cat_object).is_assignable_to(&t(&db, file_id, this_cat)),
            "reverse *Object → ThisObject direction must be rejected — preserves provenance"
        );

        let cat_other = metadata_ref(&db, MetadataKind::CatalogObject, "Номенклатура");
        assert!(!t(&db, file_id, this_cat).is_assignable_to(&t(&db, file_id, cat_other)));
    }

    #[test]
    fn is_assignable_concrete_ref_to_union_of_refs() {
        let (db, file_id) = empty_db();
        let cat_t = metadata_ref(&db, MetadataKind::CatalogRef, "Товары");
        let doc_z = metadata_ref(&db, MetadataKind::DocumentRef, "Заказ");
        let cat_o = metadata_ref(&db, MetadataKind::CatalogRef, "Номенклатура");
        let target = db.union(vec![cat_t, doc_z]);

        assert!(t(&db, file_id, cat_t).is_assignable_to(&t(&db, file_id, target)));
        assert!(t(&db, file_id, doc_z).is_assignable_to(&t(&db, file_id, target)));
        assert!(
            !t(&db, file_id, cat_o).is_assignable_to(&t(&db, file_id, target)),
            "concrete ref not present in union must be rejected"
        );
    }

    #[test]
    fn is_assignable_null_to_union_containing_ref() {
        let (db, file_id) = empty_db();
        let cat_x = metadata_ref(&db, MetadataKind::CatalogRef, "X");
        let doc_y = metadata_ref(&db, MetadataKind::DocumentRef, "Y");
        let target = db.union(vec![cat_x, doc_y]);
        assert!(t(&db, file_id, db.null()).is_assignable_to(&t(&db, file_id, target)));

        let ns = db.union(vec![db.number(None, None), db.string(None, false)]);
        assert!(!t(&db, file_id, db.null()).is_assignable_to(&t(&db, file_id, ns)));
    }

    #[test]
    fn is_assignable_union_with_null_left_distributes() {
        let (db, file_id) = empty_db();
        let cat_x = metadata_ref(&db, MetadataKind::CatalogRef, "X");
        let nullable_cat = db.union(vec![db.null(), cat_x]);
        assert!(t(&db, file_id, nullable_cat).is_assignable_to(&t(&db, file_id, cat_x)));

        let null_or_string = db.union(vec![db.null(), db.string(None, false)]);
        assert!(
            !t(&db, file_id, null_or_string).is_assignable_to(&t(&db, file_id, cat_x)),
            "union-left must reject when any component fails"
        );
    }

    #[test]
    fn is_assignable_unknown_inside_union_collapses_to_concrete_arm() {
        let (db, file_id) = empty_db();

        let unknown_or_string = db.union(vec![db.unknown(), db.string(None, false)]);
        assert_eq!(unknown_or_string, db.string(None, false));
        assert!(t(&db, file_id, unknown_or_string).is_assignable_to(&t(
            &db,
            file_id,
            db.string(None, false)
        )));

        let number_or_unknown = db.union(vec![db.number(None, None), db.unknown()]);
        assert_eq!(number_or_unknown, db.number(None, None));
        assert!(
            !t(&db, file_id, db.string(None, false))
                .is_assignable_to(&t(&db, file_id, number_or_unknown)),
            "kernel absorbs Unknown: Number|Unknown collapses to Number, so String is not assignable"
        );
    }

    #[test]
    fn is_assignable_function_reflexive_and_disjoint_primitives_fail() {
        let (db, file_id) = empty_db();
        let f_num_to_str = fixed_function(&db, vec![db.number(None, None)], db.string(None, false));
        let f_num_to_str_2 =
            fixed_function(&db, vec![db.number(None, None)], db.string(None, false));
        let f_str_to_str =
            fixed_function(&db, vec![db.string(None, false)], db.string(None, false));
        let f_num_to_num = fixed_function(&db, vec![db.number(None, None)], db.number(None, None));

        assert!(t(&db, file_id, f_num_to_str).is_assignable_to(&t(&db, file_id, f_num_to_str_2)));
        assert!(!t(&db, file_id, f_num_to_str).is_assignable_to(&t(&db, file_id, f_str_to_str)));
        assert!(!t(&db, file_id, f_num_to_str).is_assignable_to(&t(&db, file_id, f_num_to_num)));
    }

    #[test]
    fn is_assignable_function_variance_surfaces_through_facade() {
        let (db, file_id) = empty_db();
        let number_or_string = db.union(vec![db.number(None, None), db.string(None, false)]);
        let from = fixed_function(&db, vec![number_or_string], db.number(None, None));
        let to = fixed_function(&db, vec![db.number(None, None)], number_or_string);
        assert!(t(&db, file_id, from).is_assignable_to(&t(&db, file_id, to)));
        assert!(!t(&db, file_id, to).is_assignable_to(&t(&db, file_id, from)));
    }

    #[test]
    fn fields_deduplicate_duplicate_names_preferring_attributes() {
        let fixture = TempFixture::duplicated_field();
        let (db, file_id) = db_with_configuration(fixture.path());
        let cat = t(&db, file_id, metadata_ref(&db, MetadataKind::CatalogRef, "Справочник1"));

        let fields = cat.fields();
        let matches: Vec<_> =
            fields.iter().filter(|field| field.name == Name::new("Реквизит2")).collect();
        assert_eq!(matches.len(), 1, "duplicate Russian names must be deduplicated");
        assert_eq!(matches[0].ty, db.number(None, None), "attribute must win over tabular section");
    }
}
