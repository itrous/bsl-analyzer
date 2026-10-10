use std::fmt::Write;

use crate::facet::{
    ArrayFacet, DateFacet, FormBindingFacet, FormBindingTargetFacet, FunctionFacet, MapFacet,
    MdoRefFacet, MetaObjFacet, MetaRefFacet, NumberFacet, PlatformObjectFacet, ProjectionFacet,
    StringFacet, StructureFacet, TableFacet,
};
use crate::intern::TypeKernelDb;
use crate::kind::{MetadataKind, MetadataReferenceKind, Projection, TypeId, TypeKind};
use bsl_metadata::MdoType;

/// Имя платформенного типа хранится в каноническом русском написании
/// (см. `intern::canonical_platform_name`), поэтому английская локаль обязана
/// брать парное имя из корпуса — иначе она показывала бы русское. Имя вне
/// корпуса парного не имеет и печатается как есть.
fn platform_object_label(name: &str, locale: Locale) -> &str {
    let platform = bsl_platform::PlatformData::instance();
    // Хранимое имя каноническое, но не обязательно русское: у типов-тёзок
    // интернирование вынуждено сохранять английский алиас, иначе они склеились
    // бы в один тип. Поэтому обе локали спрашивают корпус, а не полагаются на то,
    // что в фасете лежит русское написание.
    match locale {
        Locale::Ru => match platform.get_type(name) {
            // Русское имя у тёзок общее, так что выбор записи здесь безопасен.
            Some(ty) if !ty.name.is_empty() => ty.name.as_str(),
            _ => name,
        },
        Locale::En => {
            // А вот английские имена у тёзок разные, и по общему русскому имени
            // нельзя выбрать одно, не приписав типу чужой перевод.
            if platform.is_ambiguous_type_name(name) {
                return name;
            }
            match platform.get_type(name) {
                Some(ty) if !ty.english_name.is_empty() => ty.english_name.as_str(),
                _ => name,
            }
        }
    }
}

fn manager_collection_label(mdo: MdoType, locale: Locale) -> &'static str {
    match locale {
        Locale::Ru => mdo.manager_type_prefix_ru().unwrap_or("МенеджерКоллекция"),
        Locale::En => mdo.manager_type_prefix().unwrap_or("ManagerCollection"),
    }
}

fn metadata_reference_collection_label(
    kind: MetadataReferenceKind,
    locale: Locale,
) -> &'static str {
    match locale {
        Locale::Ru => kind.russian_plural(),
        Locale::En => kind.english_plural(),
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum Locale {
    Ru,
    En,
}

pub trait DisplayCtx {
    fn locale(&self) -> Locale;

    fn precision_visible(&self) -> bool;
}

pub struct PlainDisplayCtx {
    pub locale: Locale,
    pub precision_visible: bool,
}

impl PlainDisplayCtx {
    pub fn hover_ru() -> Self {
        Self { locale: Locale::Ru, precision_visible: true }
    }

    pub fn hover_en() -> Self {
        Self { locale: Locale::En, precision_visible: true }
    }

    pub fn completion_ru() -> Self {
        Self { locale: Locale::Ru, precision_visible: false }
    }
}

impl DisplayCtx for PlainDisplayCtx {
    fn locale(&self) -> Locale {
        self.locale
    }

    fn precision_visible(&self) -> bool {
        self.precision_visible
    }
}

pub fn display_name(kind: &TypeKind, ctx: &dyn DisplayCtx, db: &dyn TypeKernelDb) -> String {
    let mut buf = String::new();
    render(kind, ctx, db, &mut buf);
    buf
}

fn render(kind: &TypeKind, ctx: &dyn DisplayCtx, db: &dyn TypeKernelDb, buf: &mut String) {
    match kind {
        TypeKind::Unknown => buf.push_str(match ctx.locale() {
            Locale::Ru => "Неизвестно",
            Locale::En => "Unknown",
        }),
        TypeKind::Never => buf.push_str(match ctx.locale() {
            Locale::Ru => "Никогда",
            Locale::En => "Never",
        }),
        TypeKind::Any => buf.push_str(match ctx.locale() {
            Locale::Ru => "Произвольный",
            Locale::En => "Any",
        }),
        TypeKind::Boolean => buf.push_str(match ctx.locale() {
            Locale::Ru => "Булево",
            Locale::En => "Boolean",
        }),
        TypeKind::Null => buf.push_str("NULL"),
        TypeKind::Undefined => buf.push_str(match ctx.locale() {
            Locale::Ru => "Неопределено",
            Locale::En => "Undefined",
        }),
        TypeKind::Uuid => buf.push_str(match ctx.locale() {
            Locale::Ru => "УникальныйИдентификатор",
            Locale::En => "UUID",
        }),
        TypeKind::Number(facet) => render_number(facet, ctx, buf),
        TypeKind::String(facet) => render_string(facet, ctx, buf),
        TypeKind::Date(facet) => render_date(facet, ctx, buf),
        TypeKind::Array(facet) => render_array(facet, ctx, db, buf),
        TypeKind::Map(facet) => render_map(facet, ctx, db, buf),
        TypeKind::Structure(facet) => render_structure(facet, ctx, buf),
        TypeKind::ValueList(elem) => {
            buf.push_str(match ctx.locale() {
                Locale::Ru => "СписокЗначений",
                Locale::En => "ValueList",
            });
            if let Some(id) = elem {
                buf.push_str(match ctx.locale() {
                    Locale::Ru => " из ",
                    Locale::En => " of ",
                });
                render(db.lookup_type(*id), ctx, db, buf);
            }
        }
        TypeKind::ValueTable(facet) => render_table(facet, ctx, db, false, buf),
        TypeKind::ValueTableRow(facet) => render_table(facet, ctx, db, true, buf),
        TypeKind::ValueStorage => buf.push_str(match ctx.locale() {
            Locale::Ru => "ХранилищеЗначения",
            Locale::En => "ValueStorage",
        }),
        TypeKind::TypeDescriptor => buf.push_str(match ctx.locale() {
            Locale::Ru => "Тип",
            Locale::En => "Type",
        }),
        TypeKind::PlatformObject(PlatformObjectFacet { name }) => {
            buf.push_str(platform_object_label(name, ctx.locale()));
        }
        TypeKind::MetadataRef(facet) => render_meta_ref(facet, ctx, buf),
        TypeKind::MetadataObject(facet) => render_meta_obj(facet, ctx, buf),
        TypeKind::AnyMetadataRef { mdo_type } => match MetadataKind::ref_kind_for(*mdo_type) {
            Some(kind) => buf.push_str(kind.display_label(ctx.locale())),
            None => buf.push_str(manager_collection_label(*mdo_type, ctx.locale())),
        },
        TypeKind::MetadataReferenceCollection(kind) => {
            buf.push_str(metadata_reference_collection_label(*kind, ctx.locale()));
        }
        TypeKind::MetadataObjectCollection(_) => {
            buf.push_str(platform_object_label("КоллекцияОбъектовМетаданных", ctx.locale()));
        }
        TypeKind::MetadataReference { kind, name } => {
            let kind_label = match ctx.locale() {
                Locale::Ru => kind.russian_singular(),
                Locale::En => kind.english_singular(),
            };
            buf.push_str(kind_label);
            buf.push('.');
            buf.push_str(name);
        }
        TypeKind::AnyRef => buf.push_str(match ctx.locale() {
            Locale::Ru => "ЛюбаяСсылка",
            Locale::En => "AnyRef",
        }),
        TypeKind::ManagerCollection(mdo_type) => {
            buf.push_str(manager_collection_label(*mdo_type, ctx.locale()));
        }
        TypeKind::ObjectManager(facet) => {
            let kind_label = match ctx.locale() {
                Locale::Ru => facet.mdo.russian_name(),
                Locale::En => facet.mdo.english_name(),
            };
            write!(buf, "{}.{}", kind_label, facet.name).unwrap();
        }
        TypeKind::TabularSection { parent, name } => {
            buf.push_str(match ctx.locale() {
                Locale::Ru => "ТабличнаяЧасть<",
                Locale::En => "TabularSection<",
            });
            write!(buf, "{}.{}", parent.name, name).unwrap();
            buf.push('>');
        }
        TypeKind::TabularSectionRow { parent, name } => {
            buf.push_str(match ctx.locale() {
                Locale::Ru => "СтрокаТабличнойЧасти<",
                Locale::En => "TabularSectionRow<",
            });
            write!(buf, "{}.{}", parent.name, name).unwrap();
            buf.push('>');
        }
        TypeKind::RegisterDimension { parent, name } => {
            let label = match ctx.locale() {
                Locale::Ru => "Измерение",
                Locale::En => "Dimension",
            };
            write!(buf, "{}<{}.{}>", label, parent.name, name).unwrap();
        }
        TypeKind::RegisterResource { parent, name } => {
            let label = match ctx.locale() {
                Locale::Ru => "Ресурс",
                Locale::En => "Resource",
            };
            write!(buf, "{}<{}.{}>", label, parent.name, name).unwrap();
        }
        TypeKind::RegisterAttribute { parent, name } => {
            let label = match ctx.locale() {
                Locale::Ru => "Реквизит",
                Locale::En => "Attribute",
            };
            write!(buf, "{}<{}.{}>", label, parent.name, name).unwrap();
        }
        TypeKind::RegisterFilter { .. } => buf.push_str(match ctx.locale() {
            Locale::Ru => "Отбор",
            Locale::En => "Filter",
        }),
        TypeKind::Attribute { parent, name } => {
            write!(buf, "{}.{}", parent.name, name).unwrap();
        }
        TypeKind::FormData { kind, underlying } => {
            buf.push_str(kind.platform_type_name());
            if let Some(owner) = underlying {
                buf.push(':');
                render_mdo_ref(owner, ctx, buf);
            }
        }
        TypeKind::FormControl { kind, binding } => {
            buf.push_str(kind.base_platform_type_name().unwrap_or(match ctx.locale() {
                Locale::Ru => "ЭлементФормы",
                Locale::En => "FormControl",
            }));
            if let Some(binding) = binding {
                render_form_binding(binding, ctx, db, buf);
            }
        }
        TypeKind::ThisObject { owner, .. } => {
            buf.push_str(match ctx.locale() {
                Locale::Ru => "ЭтотОбъект",
                Locale::En => "ThisObject",
            });
            buf.push(':');
            render_mdo_ref(owner, ctx, buf);
        }
        TypeKind::ThisManager { owner, .. } => {
            buf.push_str(match ctx.locale() {
                Locale::Ru => "ЭтотМенеджер",
                Locale::En => "ThisManager",
            });
            buf.push(':');
            render_mdo_ref(owner, ctx, buf);
        }
        TypeKind::CommonModule(facet) => {
            buf.push_str(match ctx.locale() {
                Locale::Ru => "ОбщийМодуль",
                Locale::En => "CommonModule",
            });
            buf.push(':');
            buf.push_str(&facet.name);
        }
        TypeKind::Union(members) => render_union(members.as_ref(), ctx, db, buf),
        TypeKind::Function(facet) => render_function(facet, ctx, db, buf),
        TypeKind::QueryResult(facet) => render_query_result(facet, ctx, db, buf),
        TypeKind::QueryResultSelection(facet) => {
            buf.push_str(match ctx.locale() {
                Locale::Ru => "ВыборкаИзРезультатаЗапроса",
                Locale::En => "QueryResultSelection",
            });
            render_projection_suffix(&facet.projection, ctx, db, buf);
        }
        TypeKind::QueryBatchResult { .. } => buf.push_str(match ctx.locale() {
            Locale::Ru => "ПакетРезультатовЗапроса",
            Locale::En => "QueryBatchResult",
        }),
        TypeKind::Query { .. } => buf.push_str(match ctx.locale() {
            Locale::Ru => "Запрос",
            Locale::En => "Query",
        }),
    }
}

fn render_number(facet: &NumberFacet, ctx: &dyn DisplayCtx, buf: &mut String) {
    buf.push_str(match ctx.locale() {
        Locale::Ru => "Число",
        Locale::En => "Number",
    });
    if ctx.precision_visible() {
        match (facet.precision, facet.scale) {
            (Some(p), Some(s)) => {
                write!(buf, "({}, {})", p, s).unwrap();
            }
            (Some(p), None) => {
                write!(buf, "({})", p).unwrap();
            }
            _ => {}
        }
    }
}

fn render_string(facet: &StringFacet, ctx: &dyn DisplayCtx, buf: &mut String) {
    buf.push_str(match ctx.locale() {
        Locale::Ru => "Строка",
        Locale::En => "String",
    });
    if ctx.precision_visible() {
        if let Some(len) = facet.length {
            write!(buf, "({})", len).unwrap();
        }
    }
}

fn render_date(facet: &DateFacet, ctx: &dyn DisplayCtx, buf: &mut String) {
    use crate::facet::DateComponent;
    buf.push_str(match (ctx.locale(), facet.component) {
        (Locale::Ru, DateComponent::Date) => "Дата",
        (Locale::Ru, DateComponent::Time) => "Время",
        (Locale::Ru, DateComponent::DateTime) => "ДатаВремя",
        (Locale::En, DateComponent::Date) => "Date",
        (Locale::En, DateComponent::Time) => "Time",
        (Locale::En, DateComponent::DateTime) => "DateTime",
    });
}

fn render_array(facet: &ArrayFacet, ctx: &dyn DisplayCtx, db: &dyn TypeKernelDb, buf: &mut String) {
    buf.push_str(match ctx.locale() {
        Locale::Ru => "Массив",
        Locale::En => "Array",
    });
    if let Some(elem) = facet.element {
        buf.push_str(match ctx.locale() {
            Locale::Ru => " из ",
            Locale::En => " of ",
        });
        render(db.lookup_type(elem), ctx, db, buf);
    }
}

fn render_map(facet: &MapFacet, ctx: &dyn DisplayCtx, db: &dyn TypeKernelDb, buf: &mut String) {
    buf.push_str(match ctx.locale() {
        Locale::Ru => "Соответствие",
        Locale::En => "Map",
    });
    if facet.key.is_some() || facet.value.is_some() {
        buf.push('<');
        render_optional(facet.key, ctx, db, buf);
        buf.push_str(", ");
        render_optional(facet.value, ctx, db, buf);
        buf.push('>');
    }
}

fn render_structure(facet: &StructureFacet, ctx: &dyn DisplayCtx, buf: &mut String) {
    buf.push_str(match ctx.locale() {
        Locale::Ru => "Структура",
        Locale::En => "Structure",
    });
    if let Some(keys) = &facet.keys {
        buf.push('(');
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                buf.push_str(", ");
            }
            buf.push_str(k);
        }
        buf.push(')');
    }
}

fn render_table(
    facet: &TableFacet,
    ctx: &dyn DisplayCtx,
    db: &dyn TypeKernelDb,
    is_row: bool,
    buf: &mut String,
) {
    let base = match (ctx.locale(), is_row) {
        (Locale::Ru, false) => "ТаблицаЗначений",
        (Locale::Ru, true) => "СтрокаТаблицыЗначений",
        (Locale::En, false) => "ValueTable",
        (Locale::En, true) => "ValueTableRow",
    };
    buf.push_str(base);
    render_projection_suffix(&facet.projection, ctx, db, buf);
}

fn render_projection_suffix(
    projection: &Option<std::sync::Arc<Projection>>,
    ctx: &dyn DisplayCtx,
    db: &dyn TypeKernelDb,
    buf: &mut String,
) {
    let Some(proj) = projection else { return };
    if !ctx.precision_visible() {
        return;
    }
    buf.push_str(" { ");
    for (i, field) in proj.fields.iter().enumerate() {
        if i > 0 {
            buf.push_str(", ");
        }
        buf.push_str(&field.name);
        buf.push_str(": ");
        match proj.raw_sdbl_types.as_deref().and_then(|shadows| shadows.get(i)) {
            Some(shadow) => buf.push_str(&shadow.display),
            None => render(db.lookup_type(field.ty), ctx, db, buf),
        }
    }
    buf.push_str(" }");
}

fn render_meta_ref(facet: &MetaRefFacet, ctx: &dyn DisplayCtx, buf: &mut String) {
    buf.push_str(facet.kind.display_label(ctx.locale()));
    buf.push('.');
    buf.push_str(&facet.name);
}

fn render_meta_obj(facet: &MetaObjFacet, ctx: &dyn DisplayCtx, buf: &mut String) {
    buf.push_str(facet.kind.display_label(ctx.locale()));
    buf.push('.');
    buf.push_str(&facet.name);
}

fn render_mdo_ref(facet: &MdoRefFacet, ctx: &dyn DisplayCtx, buf: &mut String) {
    buf.push_str(match ctx.locale() {
        Locale::Ru => facet.mdo_type.russian_name(),
        Locale::En => facet.mdo_type.english_name(),
    });
    buf.push('.');
    buf.push_str(&facet.name);
}

fn render_form_binding(
    binding: &FormBindingFacet,
    ctx: &dyn DisplayCtx,
    db: &dyn TypeKernelDb,
    buf: &mut String,
) {
    buf.push(':');
    if !binding.path.is_empty() {
        for (i, segment) in binding.path.iter().enumerate() {
            if i > 0 {
                buf.push('.');
            }
            buf.push_str(segment);
        }
        buf.push_str(" -> ");
    }
    render_form_binding_target(&binding.target, ctx, db, buf);
}

fn render_form_binding_target(
    target: &FormBindingTargetFacet,
    ctx: &dyn DisplayCtx,
    db: &dyn TypeKernelDb,
    buf: &mut String,
) {
    match target {
        FormBindingTargetFacet::TabularSection { mdo_ref, section } => {
            render_mdo_ref(mdo_ref, ctx, buf);
            buf.push('.');
            buf.push_str(section);
        }
        FormBindingTargetFacet::Attribute { ty } => render(db.lookup_type(*ty), ctx, db, buf),
    }
}

fn render_union(members: &[TypeId], ctx: &dyn DisplayCtx, db: &dyn TypeKernelDb, buf: &mut String) {
    for (i, &m) in members.iter().enumerate() {
        if i > 0 {
            buf.push_str(" | ");
        }
        render(db.lookup_type(m), ctx, db, buf);
    }
}

fn render_function(
    facet: &FunctionFacet,
    ctx: &dyn DisplayCtx,
    db: &dyn TypeKernelDb,
    buf: &mut String,
) {
    buf.push_str(match ctx.locale() {
        Locale::Ru => "Функция(",
        Locale::En => "Function(",
    });
    for (i, p) in facet.params.iter().enumerate() {
        if i > 0 {
            buf.push_str(", ");
        }
        buf.push_str(&p.name);
        buf.push_str(": ");
        render(db.lookup_type(p.ty), ctx, db, buf);
    }
    buf.push_str(") -> ");
    render(db.lookup_type(facet.returns), ctx, db, buf);
}

fn render_query_result(
    facet: &ProjectionFacet,
    ctx: &dyn DisplayCtx,
    db: &dyn TypeKernelDb,
    buf: &mut String,
) {
    buf.push_str(match ctx.locale() {
        Locale::Ru => "РезультатЗапроса",
        Locale::En => "QueryResult",
    });
    render_projection_suffix(&facet.projection, ctx, db, buf);
}

fn render_optional(
    id: Option<TypeId>,
    ctx: &dyn DisplayCtx,
    db: &dyn TypeKernelDb,
    buf: &mut String,
) {
    match id {
        Some(id) => render(db.lookup_type(id), ctx, db, buf),
        None => buf.push('?'),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bsl_metadata::MdoType;
    use expect_test::expect;

    use super::*;
    use crate::builders::Builders;
    use crate::facet::{
        DateComponent, FormBindingFacet, FormBindingTargetFacet, FormDataFacet, FormElementFacet,
        MdoRefFacet,
    };
    use crate::kind::{ConfigId, MetadataKind, ProjectionFieldSource, ProjectionOrigin};
    use crate::testing::{InMemoryDb, RootConfigCtx};

    fn ru() -> PlainDisplayCtx {
        PlainDisplayCtx::hover_ru()
    }

    fn en() -> PlainDisplayCtx {
        PlainDisplayCtx::hover_en()
    }

    fn show(db: &InMemoryDb, id: TypeId, ctx: &dyn DisplayCtx) -> String {
        display_name(db.lookup_type(id), ctx, db)
    }

    #[test]
    fn platform_object_follows_locale() {
        let data = bsl_platform::PlatformData::instance();
        let Some(reader) = data.get_type("ЧтениеТекста") else {
            return;
        };
        if reader.english_name.is_empty() {
            return;
        }
        let db = InMemoryDb::new();
        let id = db.platform_object("ЧтениеТекста".to_string());
        assert_eq!(show(&db, id, &ru()), "ЧтениеТекста");
        assert_eq!(
            show(&db, id, &en()),
            reader.english_name.as_str(),
            "имя хранится каноническим русским, поэтому английская локаль обязана \
             брать парное имя из корпуса, а не показывать русское"
        );
    }

    /// У тёзок английские имена разные, и выбрать одно значит приписать типу
    /// чужой перевод. Раз канонизация от такого выбора отказалась, отображение
    /// тем более не вправе его делать.
    #[test]
    fn ambiguous_name_is_not_translated_through_a_twin() {
        let data = bsl_platform::PlatformData::instance();
        if !data.is_ambiguous_type_name("ЭлементыФормы") {
            return;
        }
        let db = InMemoryDb::new();
        let id = db.platform_object("ЭлементыФормы".to_string());
        assert_eq!(show(&db, id, &en()), "ЭлементыФормы");
        assert_eq!(show(&db, id, &ru()), "ЭлементыФормы");
    }

    /// Хранимое имя тёзки — английский алиас, но русская локаль обязана показать
    /// русское имя: оно у тёзок общее, и выбор записи тут ничего не искажает.
    #[test]
    fn twin_alias_follows_ru_locale() {
        let data = bsl_platform::PlatformData::instance();
        if !data.is_ambiguous_type_name("ЭлементыФормы") {
            return;
        }
        let db = InMemoryDb::new();
        let id = db.platform_object("FormItems".to_string());
        assert_eq!(show(&db, id, &ru()), "ЭлементыФормы");
        assert_eq!(show(&db, id, &en()), "FormItems", "в английской алиас сохраняется");
    }

    /// Имени вне корпуса парного нет — печатается как записано, в обеих локалях.
    #[test]
    fn uncorpused_platform_object_is_shown_as_written() {
        let db = InMemoryDb::new();
        let id = db.platform_object("ДокументМенеджер".to_string());
        assert_eq!(show(&db, id, &ru()), "ДокументМенеджер");
        assert_eq!(show(&db, id, &en()), "ДокументМенеджер");
    }

    #[test]
    fn primitives_ru_and_en() {
        let db = InMemoryDb::new();
        expect!["Булево"].assert_eq(&show(&db, db.boolean(), &ru()));
        expect!["Boolean"].assert_eq(&show(&db, db.boolean(), &en()));
        expect!["Неизвестно"].assert_eq(&show(&db, db.unknown(), &ru()));
        expect!["Unknown"].assert_eq(&show(&db, db.unknown(), &en()));
        expect!["Произвольный"].assert_eq(&show(&db, db.any(), &ru()));
        expect!["Any"].assert_eq(&show(&db, db.any(), &en()));
        expect!["NULL"].assert_eq(&show(&db, db.null(), &ru()));
        expect!["Неопределено"].assert_eq(&show(&db, db.undefined(), &ru()));
    }

    #[test]
    fn number_with_precision_hover_vs_completion() {
        let db = InMemoryDb::new();
        let id = db.number(Some(15), Some(2));
        expect!["Число(15, 2)"].assert_eq(&show(&db, id, &PlainDisplayCtx::hover_ru()));
        expect!["Число"].assert_eq(&show(&db, id, &PlainDisplayCtx::completion_ru()));
        expect!["Number(15, 2)"].assert_eq(&show(&db, id, &PlainDisplayCtx::hover_en()));
        let p = db.number(Some(10), None);
        expect!["Число(10)"].assert_eq(&show(&db, p, &PlainDisplayCtx::hover_ru()));
    }

    #[test]
    fn string_length_hover_only() {
        let db = InMemoryDb::new();
        let id = db.string(Some(50), false);
        expect!["Строка(50)"].assert_eq(&show(&db, id, &PlainDisplayCtx::hover_ru()));
        expect!["String(50)"].assert_eq(&show(&db, id, &PlainDisplayCtx::hover_en()));
        expect!["Строка"].assert_eq(&show(&db, id, &PlainDisplayCtx::completion_ru()));
    }

    #[test]
    fn date_components() {
        let db = InMemoryDb::new();
        expect!["Дата"].assert_eq(&show(&db, db.date(DateComponent::Date), &ru()));
        expect!["Время"].assert_eq(&show(&db, db.date(DateComponent::Time), &ru()));
        expect!["ДатаВремя"].assert_eq(&show(&db, db.date(DateComponent::DateTime), &ru()));
        expect!["DateTime"].assert_eq(&show(&db, db.date(DateComponent::DateTime), &en()));
    }

    #[test]
    fn metadata_ref_bilingual() {
        let db = InMemoryDb::new();
        let cfg = RootConfigCtx;
        let cat = db.metadata_ref(MetadataKind::CatalogRef, "Номенклатура".to_string(), &cfg);
        expect!["СправочникСсылка.Номенклатура"].assert_eq(&show(&db, cat, &ru()));
        expect!["CatalogRef.Номенклатура"].assert_eq(&show(&db, cat, &en()));
    }

    #[test]
    fn metadata_ref_tabular_section_uses_label_not_debug() {
        let db = InMemoryDb::new();
        let cfg = RootConfigCtx;
        let ts = db.metadata_ref(
            MetadataKind::TabularSection { parent: MdoType::Catalog },
            "Номенклатура.Товары".to_string(),
            &cfg,
        );
        expect!["ТабличнаяЧасть.Номенклатура.Товары"].assert_eq(&show(&db, ts, &ru()));
        expect!["TabularSection.Номенклатура.Товары"].assert_eq(&show(&db, ts, &en()));
    }

    #[test]
    fn any_ref_renders_localized_label() {
        let db = InMemoryDb::new();
        expect!["ЛюбаяСсылка"].assert_eq(&show(&db, db.any_ref(), &ru()));
        expect!["AnyRef"].assert_eq(&show(&db, db.any_ref(), &en()));
    }

    #[test]
    fn any_metadata_ref_renders_ref_kind_label() {
        let db = InMemoryDb::new();
        let any_catalog = db.any_metadata_ref(MdoType::Catalog);
        expect!["СправочникСсылка"].assert_eq(&show(&db, any_catalog, &ru()));
        expect!["CatalogRef"].assert_eq(&show(&db, any_catalog, &en()));
    }

    #[test]
    fn tabular_section_label_is_parent_qualified() {
        use crate::kind::MetadataKind;
        let db = InMemoryDb::new();
        let cfg = RootConfigCtx;
        let parent = db.meta_ref_facet(MetadataKind::CatalogRef, "Номенклатура".to_string(), &cfg);
        let ts = db.tabular_section(parent.clone(), "Товары".to_string());
        let row = db.tabular_section_row(parent, "Товары".to_string());
        expect!["ТабличнаяЧасть<Номенклатура.Товары>"].assert_eq(&show(&db, ts, &ru()));
        expect!["TabularSection<Номенклатура.Товары>"].assert_eq(&show(&db, ts, &en()));
        expect!["СтрокаТабличнойЧасти<Номенклатура.Товары>"].assert_eq(&show(&db, row, &ru()));
    }

    #[test]
    fn array_of_element() {
        let db = InMemoryDb::new();
        let n = db.number(None, None);
        let arr = db.array(Some(n));
        expect!["Массив из Число"].assert_eq(&show(&db, arr, &ru()));
        expect!["Array of Number"].assert_eq(&show(&db, arr, &en()));

        let bare = db.array(None);
        expect!["Массив"].assert_eq(&show(&db, bare, &ru()));
    }

    #[test]
    fn register_inner_variants_use_ctx_locale() {
        use crate::kind::MetadataKind;
        let db = InMemoryDb::new();
        let cfg = RootConfigCtx;
        let parent =
            db.meta_ref_facet(MetadataKind::InformationRegisterRecordSet, "Цены".to_string(), &cfg);
        let dim = db.register_dimension(parent.clone(), "Период".to_string());
        let res = db.register_resource(parent.clone(), "Сумма".to_string());
        let att = db.register_attribute(parent.clone(), "Комментарий".to_string());
        let filt = db.register_filter(parent);

        expect!["Измерение<Цены.Период>"].assert_eq(&show(&db, dim, &ru()));
        expect!["Dimension<Цены.Период>"].assert_eq(&show(&db, dim, &en()));
        expect!["Ресурс<Цены.Сумма>"].assert_eq(&show(&db, res, &ru()));
        expect!["Resource<Цены.Сумма>"].assert_eq(&show(&db, res, &en()));
        expect!["Реквизит<Цены.Комментарий>"].assert_eq(&show(&db, att, &ru()));
        expect!["Attribute<Цены.Комментарий>"].assert_eq(&show(&db, att, &en()));
        expect!["Отбор"].assert_eq(&show(&db, filt, &ru()));
        expect!["Filter"].assert_eq(&show(&db, filt, &en()));
    }

    #[test]
    fn union_pipe_separated_deterministic() {
        let db = InMemoryDb::new();
        let n = db.number(None, None);
        let s = db.string(None, false);
        let u = db.union(vec![n, s]);
        let rendered = show(&db, u, &ru());
        let u2 = db.union(vec![s, n]);
        assert_eq!(show(&db, u2, &ru()), rendered);
        assert!(rendered == "Число | Строка" || rendered == "Строка | Число", "got {:?}", rendered);
    }

    #[test]
    fn query_result_with_projection() {
        let db = InMemoryDb::new();
        let n = db.number(Some(15), Some(2));
        let s = db.string(None, false);
        let proj = db.projection_from_fields(
            vec![("Цена".to_string(), n), ("Наименование".to_string(), s)],
            ProjectionFieldSource::Column,
            ProjectionOrigin::SdblQuery,
        );
        let qr = db.query_result(Some(proj), crate::facet::ProjectionSource::Sdbl);

        expect!["РезультатЗапроса { Цена: Число(15, 2), Наименование: Строка }"].assert_eq(&show(
            &db,
            qr,
            &ru(),
        ));

        expect!["РезультатЗапроса"].assert_eq(&show(&db, qr, &PlainDisplayCtx::completion_ru()));
    }

    #[test]
    fn projection_prefers_sdbl_display_shadow() {
        use crate::facet::SdblTypeShadowFacet;
        use crate::kind::{Projection, ProjectionField};

        let db = InMemoryDb::new();
        let bare_number = db.number(None, None);
        let fields: Arc<[ProjectionField]> = Arc::from([ProjectionField::new(
            "Цена".to_string(),
            bare_number,
            ProjectionFieldSource::Column,
        )]);
        let shadows: Arc<[SdblTypeShadowFacet]> =
            Arc::from([SdblTypeShadowFacet::new("Число(15, 2)".to_string())]);
        let proj = Arc::new(Projection::new(fields, ProjectionOrigin::SdblQuery, Some(shadows)));
        let qr = db.query_result(Some(proj), crate::facet::ProjectionSource::Sdbl);

        expect!["РезультатЗапроса { Цена: Число(15, 2) }"].assert_eq(&show(&db, qr, &ru()));
    }

    #[test]
    fn function_renders_params_and_return() {
        use crate::facet::{ArgArity, FunctionFacet, FunctionOrigin, ParamPassing, ParamSpec};

        let db = InMemoryDb::new();
        let n = db.number(None, None);
        let s = db.string(None, false);

        let facet = FunctionFacet {
            params: Arc::from([
                ParamSpec {
                    name: "Цена".to_string(),
                    ty: n,
                    passing: ParamPassing::ByRef,
                    variadic: false,
                },
                ParamSpec {
                    name: "Имя".to_string(),
                    ty: s,
                    passing: ParamPassing::ByVal,
                    variadic: false,
                },
            ]),
            defaults: Arc::from([None, None]),
            min_args: 2,
            max_args: ArgArity::Fixed(2),
            returns: n,
            origin: FunctionOrigin::UserDefined,
        };
        let id = db.function(facet);
        expect!["Функция(Цена: Число, Имя: Строка) -> Число"].assert_eq(&show(&db, id, &ru()));
    }

    #[test]
    fn form_variants_render_bilingually_with_payloads() {
        let db = InMemoryDb::new();
        let owner =
            MdoRefFacet { mdo_type: MdoType::Catalog, name: "Контрагенты".to_string() };
        let form_data =
            db.mk_form_data(FormDataFacet::StructureWithCollection, Some(owner.clone()));
        expect!["ДанныеФормыСтруктураСКоллекцией:Справочник.Контрагенты"].assert_eq(&show(
            &db,
            form_data,
            &ru(),
        ));
        expect!["ДанныеФормыСтруктураСКоллекцией:Catalog.Контрагенты"].assert_eq(&show(
            &db,
            form_data,
            &en(),
        ));

        let structure = db.mk_form_data(FormDataFacet::Structure, Some(owner.clone()));
        expect!["ДанныеФормыСтруктура:Справочник.Контрагенты"].assert_eq(&show(
            &db,
            structure,
            &ru(),
        ));
        let collection = db.mk_form_data(FormDataFacet::Collection, None);
        expect!["ДанныеФормыКоллекция"].assert_eq(&show(&db, collection, &ru()));

        let binding = FormBindingFacet {
            path: Arc::from(["Объект".to_string(), "Наименование".to_string()]),
            target: FormBindingTargetFacet::Attribute { ty: db.string(Some(50), false) },
        };
        let control = db.mk_form_control(FormElementFacet::Field, Some(binding));
        expect!["ПолеФормы:Объект.Наименование -> Строка(50)"].assert_eq(&show(
            &db,
            control,
            &ru(),
        ));
        expect!["ПолеФормы:Объект.Наименование -> String(50)"].assert_eq(&show(
            &db,
            control,
            &en(),
        ));
    }

    #[test]
    fn this_variants_render_owner_bilingually() {
        let db = InMemoryDb::new();
        let owner = MdoRefFacet {
            mdo_type: MdoType::Document, name: "ЗаказКлиента".to_string()
        };
        let object = db.mk_this_object(ConfigId::Root, owner.clone());
        let manager = db.mk_this_manager(ConfigId::Root, owner);

        expect!["ЭтотОбъект:Документ.ЗаказКлиента"].assert_eq(&show(&db, object, &ru()));
        expect!["ThisObject:Document.ЗаказКлиента"].assert_eq(&show(&db, object, &en()));
        expect!["ЭтотМенеджер:Документ.ЗаказКлиента"].assert_eq(&show(&db, manager, &ru()));
        expect!["ThisManager:Document.ЗаказКлиента"].assert_eq(&show(&db, manager, &en()));
    }

    #[test]
    fn common_module_renders_name_bilingually() {
        let db = InMemoryDb::new();
        let module = db.common_module("ОбщегоНазначения".to_string(), ConfigId::Root);

        expect!["ОбщийМодуль:ОбщегоНазначения"].assert_eq(&show(&db, module, &ru()));
        expect!["CommonModule:ОбщегоНазначения"].assert_eq(&show(&db, module, &en()));
    }

    #[test]
    fn common_module_interns_by_canonical_name() {
        let db = InMemoryDb::new();
        let a = db.common_module("Утилиты".to_string(), ConfigId::Root);
        let b = db.common_module("Утилиты".to_string(), ConfigId::Root);
        assert_eq!(a, b, "same canonical name must intern to the same type");
    }

    #[test]
    fn form_control_tabular_section_binding_renders_target() {
        let db = InMemoryDb::new();
        let owner =
            MdoRefFacet { mdo_type: MdoType::Catalog, name: "Контрагенты".to_string() };
        let binding = FormBindingFacet {
            path: Arc::from(["Объект".to_string(), "Товары".to_string()]),
            target: FormBindingTargetFacet::TabularSection {
                mdo_ref: owner,
                section: "Товары".to_string(),
            },
        };
        let control = db.mk_form_control(FormElementFacet::Table, Some(binding));

        expect!["ТаблицаФормы:Объект.Товары -> Справочник.Контрагенты.Товары"].assert_eq(&show(
            &db,
            control,
            &ru(),
        ));
        expect!["ТаблицаФормы:Объект.Товары -> Catalog.Контрагенты.Товары"].assert_eq(&show(
            &db,
            control,
            &en(),
        ));
    }
}
