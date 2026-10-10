use bsl_metadata::MdoType;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum SdblType {
    Boolean,

    String {
        length: Option<u32>,
    },

    Number {
        precision: Option<u8>,
        scale: Option<u8>,
    },

    Date,

    DateTime,

    Ref(MdoRef),

    AnyRef,

    AnyObjectRef {
        mdo_type: MdoType,
    },

    Uuid,

    ValueStorage,

    DefinedType {
        name: String,
        underlying_type: Option<Box<SdblType>>,
    },

    ValueTable,

    Null,

    Aggregate(Box<SdblType>),

    Composite {
        types: Vec<SdblType>,
    },

    TabularSectionRef {
        parent_mdo_type: bsl_metadata::MdoType,
        parent_mdo_name: String,
        ts_name: String,
    },

    #[default]
    Unknown,

    Error,
}

impl SdblType {
    pub fn string() -> Self {
        Self::String { length: None }
    }

    pub fn string_with_length(length: u32) -> Self {
        Self::String { length: Some(length) }
    }

    pub fn number() -> Self {
        Self::Number { precision: None, scale: None }
    }

    pub fn number_with_precision(precision: u8, scale: u8) -> Self {
        Self::Number { precision: Some(precision), scale: Some(scale) }
    }

    pub fn reference(mdo_type: MdoType, name: impl Into<String>) -> Self {
        Self::Ref(MdoRef { mdo_type, name: name.into() })
    }

    pub fn from_attribute_type(attr_type: &bsl_metadata::AttributeType) -> Self {
        use bsl_metadata::AttributeType;

        match attr_type {
            AttributeType::String { length } => Self::String { length: *length },
            AttributeType::Number { precision, scale } => {
                Self::Number { precision: Some(*precision), scale: Some(*scale) }
            }
            AttributeType::Boolean => Self::Boolean,
            AttributeType::Date => Self::Date,
            AttributeType::DateTime => Self::DateTime,
            AttributeType::Ref { mdo_type, name } => {
                Self::Ref(MdoRef { mdo_type: *mdo_type, name: name.clone() })
            }
            AttributeType::AnyRef => Self::AnyRef,
            AttributeType::AnyObjectRef { mdo_type } => Self::AnyObjectRef { mdo_type: *mdo_type },
            AttributeType::Uuid => Self::Uuid,
            AttributeType::ValueStorage => Self::ValueStorage,
            AttributeType::DefinedType { name } => {
                Self::DefinedType { name: name.clone(), underlying_type: None }
            }
            AttributeType::Composite { types } => {
                let sdbl_types: Vec<SdblType> =
                    types.iter().map(Self::from_attribute_type).collect();

                if sdbl_types.is_empty() {
                    Self::Unknown
                } else if sdbl_types.len() == 1 {
                    sdbl_types.into_iter().next().unwrap()
                } else {
                    Self::Composite { types: sdbl_types }
                }
            }
            AttributeType::Platform(_) => Self::Unknown,
            AttributeType::PlatformNamed(_) => Self::Unknown,
            AttributeType::UnknownNamed(_) => Self::Unknown,
            // A record manager is a form-side object, never a value a query yields.
            AttributeType::InformationRegisterRecordManager { .. } => Self::Unknown,
            AttributeType::Unknown => Self::Unknown,
        }
    }

    pub fn is_unknown_or_error(&self) -> bool {
        matches!(self, Self::Unknown | Self::Error)
    }

    pub fn is_numeric(&self) -> bool {
        match self {
            Self::Number { .. } => true,
            Self::Aggregate(inner) => inner.is_numeric(),
            _ => false,
        }
    }

    pub fn is_ref(&self) -> bool {
        matches!(self, Self::Ref(_) | Self::AnyRef | Self::AnyObjectRef { .. })
    }

    /// В выгрузке конфигурации неограниченная строка кодируется явным
    /// квалификатором `Длина = 0`; `length: None` означает отсутствие
    /// квалификатора (длина неизвестна) и неограниченной строкой не считается.
    pub fn is_unlimited_string(&self) -> bool {
        match self.unwrap_aggregate() {
            Self::String { length: Some(0) } => true,
            Self::DefinedType { underlying_type: Some(inner), .. } => inner.is_unlimited_string(),
            _ => false,
        }
    }

    pub fn unwrap_aggregate(&self) -> &Self {
        match self {
            Self::Aggregate(inner) => inner.unwrap_aggregate(),
            _ => self,
        }
    }

    pub fn is_compatible_with(&self, other: &Self) -> bool {
        use SdblType::*;

        match (self.unwrap_aggregate(), other.unwrap_aggregate()) {
            (Unknown, _) | (_, Unknown) => true,
            (Error, _) | (_, Error) => true,

            (DefinedType { .. }, _) | (_, DefinedType { .. }) => true,

            (Boolean, Boolean) => true,
            (String { .. }, String { .. }) => true,
            (Number { .. }, Number { .. }) => true,
            (Date, Date) | (DateTime, DateTime) | (Date, DateTime) | (DateTime, Date) => true,
            (Null, _) | (_, Null) => true,

            (Ref(a), Ref(b)) => a.mdo_type == b.mdo_type,

            (AnyRef, Ref(_)) | (Ref(_), AnyRef) => true,
            (AnyRef, AnyRef) => true,

            (AnyObjectRef { .. }, AnyRef) | (AnyRef, AnyObjectRef { .. }) => true,
            (AnyObjectRef { mdo_type: a }, Ref(b)) | (Ref(b), AnyObjectRef { mdo_type: a }) => {
                *a == b.mdo_type
            }
            (AnyObjectRef { mdo_type: a }, AnyObjectRef { mdo_type: b }) => a == b,

            (Uuid, Uuid) => true,
            (ValueStorage, ValueStorage) => true,
            (ValueTable, ValueTable) => true,

            (Composite { types }, other) => types.iter().any(|t| t.is_compatible_with(other)),
            (other, Composite { types }) => types.iter().any(|t| other.is_compatible_with(t)),

            _ => false,
        }
    }
}

impl SdblType {
    pub fn display_name(&self, locale: base_db::Locale) -> String {
        use base_db::Locale;
        match self {
            Self::Boolean => match locale {
                Locale::Ru => "Булево".into(),
                Locale::En => "Boolean".into(),
            },
            Self::String { length } => {
                let head = match locale {
                    Locale::Ru => "Строка",
                    Locale::En => "String",
                };
                match length {
                    Some(len) => format!("{head}({len})"),
                    None => head.into(),
                }
            }
            Self::Number { precision, scale } => {
                let head = match locale {
                    Locale::Ru => "Число",
                    Locale::En => "Number",
                };
                match (precision, scale) {
                    (Some(p), Some(s)) => format!("{head}({p}, {s})"),
                    (Some(p), None) => format!("{head}({p})"),
                    _ => head.into(),
                }
            }
            Self::Date => match locale {
                Locale::Ru => "Дата".into(),
                Locale::En => "Date".into(),
            },
            Self::DateTime => match locale {
                Locale::Ru => "ДатаВремя".into(),
                Locale::En => "DateTime".into(),
            },
            Self::Ref(mdo_ref) => {
                let label = match locale {
                    Locale::Ru => mdo_ref.mdo_type.russian_name(),
                    Locale::En => mdo_ref.mdo_type.english_name(),
                };
                format!("{}.{}", label, mdo_ref.name)
            }
            Self::AnyRef => match locale {
                Locale::Ru => "ЛюбаяСсылка".into(),
                Locale::En => "AnyRef".into(),
            },
            Self::AnyObjectRef { mdo_type } => match locale {
                Locale::Ru => mdo_type.russian_name().into(),
                Locale::En => mdo_type.english_name().into(),
            },
            Self::Uuid => match locale {
                Locale::Ru => "УникальныйИдентификатор".into(),
                Locale::En => "Uuid".into(),
            },
            Self::ValueStorage => match locale {
                Locale::Ru => "ХранилищеЗначения".into(),
                Locale::En => "ValueStorage".into(),
            },
            Self::DefinedType { name, .. } => match locale {
                Locale::Ru => format!("ОпределяемыйТип.{name}"),
                Locale::En => format!("DefinedType.{name}"),
            },
            Self::ValueTable => match locale {
                Locale::Ru => "ТаблицаЗначений".into(),
                Locale::En => "ValueTable".into(),
            },
            Self::Null => "NULL".into(),
            Self::Aggregate(inner) => {
                let head = match locale {
                    Locale::Ru => "Агрегат",
                    Locale::En => "Aggregate",
                };
                format!("{head}({})", inner.display_name(locale))
            }
            Self::Composite { types } => {
                if types.is_empty() {
                    match locale {
                        Locale::Ru => "Составной тип (пусто)".into(),
                        Locale::En => "Composite type (empty)".into(),
                    }
                } else if types.len() == 1 {
                    types[0].display_name(locale)
                } else {
                    match locale {
                        Locale::Ru => "Составной тип:".into(),
                        Locale::En => "Composite type:".into(),
                    }
                }
            }
            Self::TabularSectionRef { parent_mdo_type, parent_mdo_name, ts_name } => {
                let parent_label = match locale {
                    Locale::Ru => parent_mdo_type.russian_name(),
                    Locale::En => parent_mdo_type.english_name(),
                };
                format!("{parent_label}.{parent_mdo_name}.{ts_name}")
            }
            Self::Unknown => match locale {
                Locale::Ru => "Неизвестно".into(),
                Locale::En => "Unknown".into(),
            },
            Self::Error => match locale {
                Locale::Ru => "Ошибка".into(),
                Locale::En => "Error".into(),
            },
        }
    }
}

impl std::fmt::Display for SdblType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Boolean => write!(f, "Булево"),
            Self::String { length } => {
                write!(f, "Строка")?;
                if let Some(len) = length {
                    write!(f, "({})", len)?;
                }
                Ok(())
            }
            Self::Number { precision, scale } => {
                write!(f, "Число")?;
                match (precision, scale) {
                    (Some(p), Some(s)) => write!(f, "({}, {})", p, s)?,
                    (Some(p), None) => write!(f, "({})", p)?,
                    _ => {}
                }
                Ok(())
            }
            Self::Date => write!(f, "Дата"),
            Self::DateTime => write!(f, "ДатаВремя"),
            Self::Ref(mdo_ref) => write!(f, "{}", mdo_ref),
            Self::AnyRef => write!(f, "ЛюбаяСсылка"),
            Self::AnyObjectRef { mdo_type } => {
                write!(f, "{}", mdo_type.russian_name())
            }
            Self::Uuid => write!(f, "УникальныйИдентификатор"),
            Self::ValueStorage => write!(f, "ХранилищеЗначения"),
            Self::DefinedType { name, .. } => {
                write!(f, "ОпределяемыйТип.{}", name)
            }
            Self::ValueTable => write!(f, "ТаблицаЗначений"),
            Self::Null => write!(f, "NULL"),
            Self::Aggregate(inner) => write!(f, "Агрегат({})", inner),
            Self::Composite { types } => {
                if types.is_empty() {
                    write!(f, "Составной тип (пусто)")
                } else if types.len() == 1 {
                    write!(f, "{}", types[0])
                } else {
                    write!(f, "Составной тип:")
                }
            }
            Self::TabularSectionRef { parent_mdo_type, parent_mdo_name, ts_name } => {
                write!(f, "{}.{}.{}", parent_mdo_type.russian_name(), parent_mdo_name, ts_name)
            }
            Self::Unknown => write!(f, "Неизвестно"),
            Self::Error => write!(f, "Ошибка"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MdoRef {
    pub mdo_type: MdoType,
    pub name: String,
}

impl MdoRef {
    pub fn new(mdo_type: MdoType, name: impl Into<String>) -> Self {
        Self { mdo_type, name: name.into() }
    }

    pub fn full_name(&self) -> String {
        format!("{}.{}", self.mdo_type.russian_name(), self.name)
    }
}

impl std::fmt::Display for MdoRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.mdo_type.russian_name(), self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sdbl_type_display() {
        assert_eq!(SdblType::Boolean.to_string(), "Булево");
        assert_eq!(SdblType::string().to_string(), "Строка");
        assert_eq!(SdblType::string_with_length(17).to_string(), "Строка(17)");
        assert_eq!(SdblType::number().to_string(), "Число");
        assert_eq!(SdblType::number_with_precision(10, 2).to_string(), "Число(10, 2)");
        assert_eq!(SdblType::Number { precision: Some(15), scale: None }.to_string(), "Число(15)");
        assert_eq!(SdblType::Date.to_string(), "Дата");
        assert_eq!(SdblType::AnyRef.to_string(), "ЛюбаяСсылка");
        assert_eq!(SdblType::AnyObjectRef { mdo_type: MdoType::Catalog }.to_string(), "Справочник");
        assert_eq!(SdblType::AnyObjectRef { mdo_type: MdoType::Document }.to_string(), "Документ");
        assert_eq!(
            SdblType::AnyObjectRef { mdo_type: MdoType::BusinessProcess }.to_string(),
            "БизнесПроцесс"
        );
        assert_eq!(SdblType::Uuid.to_string(), "УникальныйИдентификатор");
        assert_eq!(SdblType::ValueStorage.to_string(), "ХранилищеЗначения");

        assert_eq!(
            SdblType::DefinedType {
                name: "ОтметкаВремени".to_string(), underlying_type: None
            }
            .to_string(),
            "ОпределяемыйТип.ОтметкаВремени"
        );

        assert_eq!(
            SdblType::DefinedType {
                name: "ОтметкаВремени".to_string(),
                underlying_type: Some(Box::new(SdblType::string_with_length(17)))
            }
            .to_string(),
            "ОпределяемыйТип.ОтметкаВремени"
        );

        assert_eq!(SdblType::Unknown.to_string(), "Неизвестно");
    }

    #[test]
    fn display_name_localizes_primitive_and_parametric_forms() {
        use base_db::Locale;

        assert_eq!(SdblType::Boolean.display_name(Locale::Ru), "Булево");
        assert_eq!(SdblType::Boolean.display_name(Locale::En), "Boolean");
        assert_eq!(SdblType::string_with_length(17).display_name(Locale::Ru), "Строка(17)");
        assert_eq!(SdblType::string_with_length(17).display_name(Locale::En), "String(17)");
        assert_eq!(SdblType::number_with_precision(10, 2).display_name(Locale::Ru), "Число(10, 2)");
        assert_eq!(
            SdblType::number_with_precision(10, 2).display_name(Locale::En),
            "Number(10, 2)"
        );
        assert_eq!(
            SdblType::Number { precision: Some(15), scale: None }.display_name(Locale::Ru),
            "Число(15)"
        );
        assert_eq!(
            SdblType::Number { precision: Some(15), scale: None }.display_name(Locale::En),
            "Number(15)"
        );

        let mdo_ref = SdblType::reference(MdoType::Catalog, "Валюты");
        assert_eq!(mdo_ref.display_name(Locale::Ru), "Справочник.Валюты");
        assert_eq!(mdo_ref.display_name(Locale::En), "Catalog.Валюты");

        assert_eq!(SdblType::AnyRef.display_name(Locale::Ru), "ЛюбаяСсылка");
        assert_eq!(SdblType::AnyRef.display_name(Locale::En), "AnyRef");
        assert_eq!(SdblType::Uuid.display_name(Locale::En), "Uuid");
        assert_eq!(
            SdblType::DefinedType {
                name: "ОтметкаВремени".into(), underlying_type: None
            }
            .display_name(Locale::En),
            "DefinedType.ОтметкаВремени"
        );

        assert_eq!(SdblType::Boolean.to_string(), "Булево");
    }

    #[test]
    fn test_mdo_ref() {
        let mdo_ref = MdoRef::new(MdoType::Catalog, "Валюты");
        assert_eq!(mdo_ref.full_name(), "Справочник.Валюты");
        assert_eq!(mdo_ref.to_string(), "Справочник.Валюты");
    }

    #[test]
    fn test_type_compatibility() {
        assert!(SdblType::Boolean.is_compatible_with(&SdblType::Boolean));
        assert!(SdblType::number().is_compatible_with(&SdblType::number_with_precision(10, 2)));
        assert!(SdblType::Date.is_compatible_with(&SdblType::DateTime));
        assert!(SdblType::Unknown.is_compatible_with(&SdblType::string()));
        assert!(
            SdblType::Null.is_compatible_with(&SdblType::Number { precision: None, scale: None })
        );

        assert!(SdblType::DefinedType { name: "Test".to_string(), underlying_type: None }
            .is_compatible_with(&SdblType::string()));
        assert!(SdblType::Boolean.is_compatible_with(&SdblType::DefinedType {
            name: "Test".to_string(),
            underlying_type: None
        }));

        assert!(!SdblType::Boolean.is_compatible_with(&SdblType::string()));
        assert!(!SdblType::number().is_compatible_with(&SdblType::string()));
    }

    #[test]
    fn test_any_object_ref_compatibility() {
        use bsl_metadata::MdoType;

        let catalog_ref = SdblType::AnyObjectRef { mdo_type: MdoType::Catalog };
        let document_ref = SdblType::AnyObjectRef { mdo_type: MdoType::Document };
        let bp_ref = SdblType::AnyObjectRef { mdo_type: MdoType::BusinessProcess };

        assert!(catalog_ref.is_compatible_with(&SdblType::AnyRef));
        assert!(SdblType::AnyRef.is_compatible_with(&catalog_ref));

        let catalog_item = SdblType::reference(MdoType::Catalog, "Валюты");
        assert!(catalog_ref.is_compatible_with(&catalog_item));
        assert!(catalog_item.is_compatible_with(&catalog_ref));

        let document_item = SdblType::reference(MdoType::Document, "ПКО");
        assert!(!catalog_ref.is_compatible_with(&document_item));
        assert!(!document_item.is_compatible_with(&catalog_ref));

        let another_catalog_ref = SdblType::AnyObjectRef { mdo_type: MdoType::Catalog };
        assert!(catalog_ref.is_compatible_with(&another_catalog_ref));

        assert!(!catalog_ref.is_compatible_with(&document_ref));
        assert!(!document_ref.is_compatible_with(&bp_ref));

        assert!(!catalog_ref.is_compatible_with(&SdblType::string()));
        assert!(!catalog_ref.is_compatible_with(&SdblType::number()));
    }

    #[test]
    fn test_is_numeric() {
        assert!(SdblType::number().is_numeric());
        assert!(SdblType::number_with_precision(10, 2).is_numeric());
        assert!(SdblType::Aggregate(Box::new(SdblType::number())).is_numeric());

        assert!(!SdblType::string().is_numeric());
        assert!(!SdblType::Boolean.is_numeric());
    }

    #[test]
    fn test_composite_type_display() {
        use bsl_metadata::MdoType;

        let single = SdblType::Composite {
            types: vec![SdblType::reference(MdoType::Enum, "ВидДействия1")],
        };
        assert_eq!(single.to_string(), "Перечисление.ВидДействия1");

        let composite = SdblType::Composite {
            types: vec![
                SdblType::reference(MdoType::Enum, "ВидыУсловийОтбораИсходящихПисем"),
                SdblType::reference(MdoType::Enum, "ВидыДействийПриОбработкеИсходящихПисем"),
                SdblType::reference(MdoType::Enum, "ВидыУсловийОтбораВходящихПисем"),
                SdblType::reference(MdoType::Enum, "ВидыДействийПриОбработкеВходящихПисем"),
            ],
        };

        let display = composite.to_string();
        assert_eq!(display, "Составной тип:");
    }

    #[test]
    fn test_composite_type_compatibility() {
        use bsl_metadata::MdoType;

        let composite = SdblType::Composite {
            types: vec![
                SdblType::reference(MdoType::Enum, "Enum1"),
                SdblType::reference(MdoType::Enum, "Enum2"),
                SdblType::Boolean,
            ],
        };

        assert!(composite.is_compatible_with(&SdblType::reference(MdoType::Enum, "Enum1")));
        assert!(composite.is_compatible_with(&SdblType::reference(MdoType::Enum, "Enum2")));
        assert!(composite.is_compatible_with(&SdblType::Boolean));

        assert!(!composite.is_compatible_with(&SdblType::string()));
        assert!(!composite.is_compatible_with(&SdblType::number()));
    }
}
