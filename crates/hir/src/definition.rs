use crate::{MethodId, ModuleId, Name, VariableId};
use hir_def::DefDatabase;
use hir_ty::PlatformMethodHandle;
use std::sync::Arc;
use stdx::case::fold_lower_per_char;
use syntax::TextRange;
use vfs::FileId;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Definition {
    Method(MethodId),
    Variable(VariableId),
    Parameter { method_id: MethodId, param_name: Name, param_index: u32 },
    Local { method_id: MethodId, var_name: Name },
    BuiltinFunction(Name),

    BuiltinMethodHandle { handle: PlatformMethodHandle, method_name: Name },
    MdoCollectionType(bsl_metadata::MdoType),
    MdoObject { mdo_type: bsl_metadata::MdoType, object_name: Name },

    MdoManagerModule { mdo_type: bsl_metadata::MdoType, object_name: Name, file_id: FileId },
    Module(ModuleId),
    VirtualTableField { table_name: Name, field_name: Name },
    Unresolved,
}

impl Definition {
    /// Whether these two name the same thing in the language, rather than in the source.
    ///
    /// Several variants carry a [`Name`] spelled the way the occurrence spelled it, and BSL
    /// is case-insensitive: `Сообщить` and `СООБЩИТЬ` are one function written twice. Derived
    /// equality compares those spellings byte for byte, so a caller deduplicating by it sees
    /// two entities where the language has one — and answers with a choice between clones of
    /// one place.
    ///
    /// Only the name-carrying variants fold. The ones keyed by an id keep exact equality:
    /// two methods called `Расчёт` in two modules are genuinely two, and folding them would
    /// hide an ambiguity that is real.
    pub fn same_entity(&self, other: &Self) -> bool {
        self.folded() == other.folded()
    }

    /// This definition with every occurrence-spelled name folded: derived equality of two
    /// folded definitions is exactly [`Self::same_entity`]. A definition stored inside a key is
    /// compared and hashed only by derived `Eq` and `Hash`, so it must be stored folded.
    pub fn folded(&self) -> Definition {
        let fold = |name: &Name| Name::from(fold_lower_per_char(name.as_str()));
        match self {
            Definition::BuiltinFunction(name) => Definition::BuiltinFunction(fold(name)),
            Definition::BuiltinMethodHandle { handle, method_name } => {
                Definition::BuiltinMethodHandle {
                    handle: handle.clone(),
                    method_name: fold(method_name),
                }
            }
            Definition::MdoObject { mdo_type, object_name } => {
                Definition::MdoObject { mdo_type: *mdo_type, object_name: fold(object_name) }
            }
            Definition::MdoManagerModule { mdo_type, object_name, file_id } => {
                Definition::MdoManagerModule {
                    mdo_type: *mdo_type,
                    object_name: fold(object_name),
                    file_id: *file_id,
                }
            }
            Definition::VirtualTableField { table_name, field_name } => {
                Definition::VirtualTableField {
                    table_name: fold(table_name),
                    field_name: fold(field_name),
                }
            }
            other => other.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReferenceScope {
    FileLocal,
    ModuleSymbolWorkspace,
    /// A source UUID and owner-context proof must precede its reference walk.
    Metadata,
    Unknown,
}

impl Definition {
    pub fn module(&self, _db: &dyn DefDatabase) -> Option<ModuleId> {
        match self {
            Definition::Method(id) => Some(id.module),
            Definition::Variable(id) => Some(id.module),
            Definition::Parameter { method_id, .. } => Some(method_id.module),
            Definition::Local { method_id, .. } => Some(method_id.module),
            Definition::Module(id) => Some(*id),
            Definition::MdoManagerModule { file_id, .. } => Some(ModuleId::new(*file_id)),
            Definition::BuiltinFunction(_)
            | Definition::BuiltinMethodHandle { .. }
            | Definition::MdoCollectionType(_)
            | Definition::MdoObject { .. }
            | Definition::VirtualTableField { .. }
            | Definition::Unresolved => None,
        }
    }

    pub fn name(&self, db: &dyn DefDatabase) -> Option<Name> {
        match self {
            Definition::Method(id) => crate::get_method_info(id, db).map(|i| i.name),
            Definition::Variable(id) => crate::get_variable_info(id, db).map(|i| i.name),
            Definition::Parameter { param_name, .. } => Some(param_name.clone()),
            Definition::Local { var_name, .. } => Some(var_name.clone()),
            Definition::BuiltinFunction(name) => Some(name.clone()),
            Definition::BuiltinMethodHandle { method_name, .. } => Some(method_name.clone()),
            Definition::MdoObject { object_name, .. } => Some(object_name.clone()),
            Definition::Module(_) => None,
            Definition::VirtualTableField { field_name, .. } => Some(field_name.clone()),
            Definition::MdoCollectionType(_) | Definition::MdoManagerModule { .. } => None,
            Definition::Unresolved => None,
        }
    }

    pub fn is_export(&self, db: &dyn DefDatabase) -> bool {
        match self {
            Definition::Method(id) => crate::get_method_info(id, db).is_some_and(|i| i.is_export),
            Definition::Variable(id) => {
                crate::get_variable_info(id, db).is_some_and(|i| i.is_export)
            }
            _ => false,
        }
    }

    pub fn reference_scope(&self, db: &dyn DefDatabase) -> ReferenceScope {
        match self {
            Definition::Parameter { .. } | Definition::Local { .. } => ReferenceScope::FileLocal,
            Definition::Method(_) | Definition::Variable(_) => {
                if self.is_export(db) {
                    ReferenceScope::ModuleSymbolWorkspace
                } else {
                    ReferenceScope::FileLocal
                }
            }
            Definition::MdoObject { .. } | Definition::VirtualTableField { .. } => {
                ReferenceScope::Metadata
            }
            Definition::BuiltinFunction(_)
            | Definition::BuiltinMethodHandle { .. }
            | Definition::MdoCollectionType(_)
            | Definition::MdoManagerModule { .. }
            | Definition::Module(_)
            | Definition::Unresolved => ReferenceScope::Unknown,
        }
    }

    pub fn source_range(&self, db: &dyn DefDatabase) -> Option<TextRange> {
        match self {
            Definition::Method(id) => crate::get_method_info(id, db).map(|i| i.source_range),
            Definition::Variable(id) => crate::get_variable_info(id, db).map(|i| i.source_range),
            _ => None,
        }
    }

    pub fn name_range(&self, db: &dyn DefDatabase) -> Option<TextRange> {
        match self {
            Definition::Method(id) => crate::get_method_info(id, db).map(|i| i.name_range),
            _ => None,
        }
    }

    pub fn docs(&self, db: &dyn DefDatabase) -> Option<Arc<hir_def::docs::MethodDocs>> {
        match self {
            Definition::Method(id) => db.method_docs(*id),
            _ => None,
        }
    }

    pub fn label(&self, db: &dyn DefDatabase) -> String {
        match self {
            Definition::Method(id) => {
                let info = crate::get_method_info(id, db);
                let name = info.as_ref().map_or_else(Name::missing, |i| i.name.clone());
                if info.is_some_and(|i| i.is_function) {
                    format!("Функция {}()", name.as_str())
                } else {
                    format!("Процедура {}()", name.as_str())
                }
            }
            Definition::Variable(_) => {
                let name = self.name(db).unwrap_or_else(Name::missing);
                format!("Перем {}", name.as_str())
            }
            Definition::Parameter { param_name, .. } => {
                format!("Параметр {}", param_name.as_str())
            }
            Definition::Local { var_name, .. } => {
                format!("Локальная переменная {}", var_name.as_str())
            }
            Definition::BuiltinFunction(name) => {
                format!("Builtin: {}()", name.as_str())
            }
            Definition::BuiltinMethodHandle { handle, method_name } => {
                use hir_ty::PlatformMethodOrigin;
                let qualifier = match &handle.origin {
                    PlatformMethodOrigin::Scalar { type_name } => type_name.to_string(),
                    PlatformMethodOrigin::Prefixed { mdo_type, mdo_name, .. } => {
                        format!("{}.{}", mdo_type.russian_name(), mdo_name.as_str())
                    }
                };
                format!("{}.{}()", qualifier, method_name.as_str())
            }
            Definition::MdoCollectionType(mdo_type) => {
                format!("MDO Collection: {}", mdo_type.russian_name())
            }
            Definition::MdoObject { mdo_type, object_name } => {
                format!("{}.{}", mdo_type.russian_name(), object_name.as_str())
            }
            Definition::MdoManagerModule { mdo_type, object_name, .. } => {
                format!("Manager Module: {}.{}", mdo_type.russian_name(), object_name.as_str())
            }
            Definition::Module(_) => "Module".to_string(),
            Definition::VirtualTableField { table_name, field_name } => {
                format!("{}.{}", table_name.as_str(), field_name.as_str())
            }
            Definition::Unresolved => "<unresolved>".to_string(),
        }
    }

    pub fn file_id(&self, db: &dyn DefDatabase) -> Option<FileId> {
        self.module(db).map(|module| module.file_id)
    }

    pub fn is_method(&self) -> bool {
        matches!(self, Definition::Method(_))
    }

    pub fn is_variable(&self) -> bool {
        matches!(self, Definition::Variable(_) | Definition::Local { .. })
    }

    pub fn is_parameter(&self) -> bool {
        matches!(self, Definition::Parameter { .. })
    }

    pub fn is_builtin(&self) -> bool {
        matches!(self, Definition::BuiltinFunction(_) | Definition::BuiltinMethodHandle { .. })
    }

    pub fn is_mdo(&self) -> bool {
        matches!(
            self,
            Definition::MdoCollectionType(_)
                | Definition::MdoObject { .. }
                | Definition::MdoManagerModule { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A folded definition is what a key stores, so two spellings of one entity must land in
    /// one hash bucket, while the id- and kind-bearing parts still tell entities apart.
    #[test]
    fn folded_definitions_are_equal_exactly_when_same_entity() {
        let catalog = |name: &str| Definition::MdoObject {
            mdo_type: bsl_metadata::MdoType::Catalog,
            object_name: Name::new(name),
        };
        let document = Definition::MdoObject {
            mdo_type: bsl_metadata::MdoType::Document,
            object_name: Name::new("Номенклатура"),
        };
        let builtin = |name: &str| Definition::BuiltinFunction(Name::new(name));
        let table = |table: &str, field: &str| Definition::VirtualTableField {
            table_name: Name::new(table),
            field_name: Name::new(field),
        };

        let pairs = [
            (catalog("Номенклатура"), catalog("НОМЕНКЛАТУРА"), true),
            (catalog("Номенклатура"), document.clone(), false),
            (catalog("Номенклатура"), catalog("Контрагенты"), false),
            (builtin("Сообщить"), builtin("сообщить"), true),
            (table("Остатки", "Количество"), table("ОСТАТКИ", "количество"), true),
            (table("Остатки", "Количество"), table("Обороты", "Количество"), false),
        ];
        for (left, right, same) in pairs {
            assert_eq!(left.same_entity(&right), same, "{left:?} vs {right:?}");
            assert_eq!(left.folded() == right.folded(), same, "{left:?} vs {right:?}");
            let bucket: HashSet<_> = [left.folded(), right.folded()].into_iter().collect();
            assert_eq!(bucket.len() == 1, same, "{left:?} vs {right:?}");
        }
    }
}
