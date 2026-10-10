use intern::NormName;
use rustc_hash::FxHashSet;
use stdx::case::CaseExt;

use crate::define_metadata;
use crate::metadata::*;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use hir::{BindingId, IdConversion};
use ide_db::TextRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[
        bsl_metadata::ModuleType::CommandModule,
        bsl_metadata::ModuleType::CommonModule,
        bsl_metadata::ModuleType::ManagerModule,
        bsl_metadata::ModuleType::ValueManagerModule,
        bsl_metadata::ModuleType::SessionModule,
        bsl_metadata::ModuleType::Unknown,
    ],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Brainoverload, MetadataTag::Badpractice, MetadataTag::Unused],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const MANAGED_FORM_TYPE_NAME: &str = "ФормаКлиентскогоПриложения";
const ORDINARY_FORM_TYPE_NAME: &str = "Форма";

fn build_attribute_names_to_skip(ctx: &DiagnosticsContext) -> FxHashSet<String> {
    let metadata = ctx.module_metadata();

    match metadata.module_type {
        bsl_metadata::ModuleType::ObjectModule => {
            let mdo = match &metadata.mdo {
                Some(mdo) => mdo,
                None => return FxHashSet::default(),
            };

            let mut names = FxHashSet::default();

            for attr in &mdo.attributes {
                names.insert(attr.name.fold_lower());
                if let Some(ref en) = attr.name_en {
                    names.insert(en.fold_lower());
                }
            }
            for common in &mdo.common_attributes {
                names.insert(common.name.fold_lower());
            }

            for ts in &mdo.tabular_sections {
                names.insert(ts.name().fold_lower());
                if let Some(en) = ts.name_en() {
                    names.insert(en.fold_lower());
                }
            }

            names
        }
        bsl_metadata::ModuleType::RecordSetModule => {
            let register = match &metadata.register {
                Some(register) => register,
                None => return FxHashSet::default(),
            };

            let mut names = FxHashSet::default();

            for dim in register.dimensions() {
                names.insert(dim.name().fold_lower());
            }
            for res in register.resources() {
                names.insert(res.name().fold_lower());
                if let Some(en) = res.name_en() {
                    names.insert(en.fold_lower());
                }
            }
            for attr in register.attributes() {
                names.insert(attr.name().fold_lower());
                if let Some(en) = attr.name_en() {
                    names.insert(en.fold_lower());
                }
            }
            for common in register.common_attributes() {
                names.insert(common.name.fold_lower());
            }

            names
        }
        bsl_metadata::ModuleType::FormModule => {
            // A bare name matching a form standard property (ТекущийЭлемент,
            // КлючУникальности, Доступность, …) refers to the form context, not a
            // local — assigning one is a side effect on the form, never a dead
            // store. Enumerate the full platform contract rather than a
            // hand-maintained subset, picking the contract that matches the form
            // kind; absent metadata defaults to the managed form (the superset).
            let form_type_name = match metadata.form.as_ref() {
                Some(form) if !form.is_managed() => ORDINARY_FORM_TYPE_NAME,
                _ => MANAGED_FORM_TYPE_NAME,
            };
            let mut names: FxHashSet<String> = bsl_platform::PlatformDataInner::instance()
                .get_type_properties(form_type_name)
                .into_iter()
                .flat_map(|prop| [prop.name.fold_lower(), prop.english_name.fold_lower()])
                .collect();

            // The managed/ordinary base above omits per-MDO form-extension
            // properties (`ВариантМодифицирован`,
            // `ПользовательскиеНастройкиМодифицированы`, …): assigning one is
            // still a write to the form context, not a dead local. Which
            // extension a form pulls in depends on its owner MDO, which is not
            // threaded here, so union every extension name; these are specific
            // reserved identifiers, unlike a bare local.
            names.extend(
                bsl_platform::PlatformDataInner::instance()
                    .form_extension_property_names()
                    .iter()
                    .map(|name| name.to_string()),
            );

            if let Some(form) = &metadata.form {
                for attr_name in form.attribute_names() {
                    names.insert(attr_name.fold_lower());
                }
            }

            names
        }
        _ => FxHashSet::default(),
    }
}

pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::UnusedLocalVariable;

    if ctx.is_disabled_with_metadata(code) {
        return vec![];
    }

    let mut diagnostics = Vec::new();

    let mut skip_attr_names = build_attribute_names_to_skip(ctx);
    // Type-aware providers also surface platform context properties
    // (БлокироватьДляИзменения, ОбменДанными, ДополнительныеСвойства, …):
    // assigning one is a side effect on the module context, not a local.
    skip_attr_names.extend(ctx.module_implicit_field_names());

    let module_bodies = ctx.module_bodies();

    // Module `Перем` variables are not method locals. Assigning one inside a
    // procedure or in module-level init code is a write to the module variable,
    // not a dead store — its unused-ness (and the export exemption) is owned by
    // check_module_var_declarations, which sees reads across every body.
    for var in module_bodies.module_vars() {
        skip_attr_names.insert(var.name.fold_lower());
    }

    for (local_id, body) in module_bodies.iter_bodies() {
        diagnostics.extend(check_method(
            local_id,
            body,
            &module_bodies,
            code,
            ctx,
            &skip_attr_names,
        ));
    }

    diagnostics.extend(check_module_level_code(code, ctx, &skip_attr_names));

    diagnostics.extend(check_module_var_declarations(&module_bodies, code, ctx));

    diagnostics
}

/// Collects the lowercased names of every local that appears in a *read*
/// position within `body`.
///
/// A bare `Expr::Path` is a read everywhere except when it is the direct target
/// of an assignment (`Имя = …`), which is a pure write. Member- and index-base
/// paths (`Имя.Поле = …`, `Имя[И] = …`) remain reads of `Имя`, matching how the
/// value of `Имя` is consumed to reach the assigned location.
///
/// This is the correct primitive for "unused local variable": a variable is
/// unused iff its name never appears in a read position. Block-boundary liveness
/// cannot answer this — a name read and then reassigned inside the same block is
/// dead at both block edges yet genuinely used.
fn collect_read_var_names(body: &hir::Body) -> FxHashSet<String> {
    let mut write_targets: FxHashSet<hir::ExprId> = FxHashSet::default();
    for (_stmt_id, stmt) in body.stmts_iter() {
        if let hir::Stmt::Assign { target, .. } = stmt {
            let target = hir::ExprId::from_idx(*target);
            if matches!(body.expr(target), hir::Expr::Path(_)) {
                write_targets.insert(target);
            }
        }
    }

    let mut read_vars: FxHashSet<String> = FxHashSet::default();
    for (expr_id, expr) in body.exprs_iter() {
        if let hir::Expr::Path(name) = expr {
            if !write_targets.contains(&expr_id) {
                read_vars.insert(name.as_str().fold_lower());
            }
        }
    }
    read_vars
}

/// Whether the bare name `name_lower` denotes a metadata manager collection —
/// cheap and purely textual, used only to decide whether the question below is
/// worth asking at all.
fn names_a_manager_collection(name_lower: &str) -> bool {
    bsl_metadata::MdoType::from_plural(name_lower)
        .is_some_and(|mdo| mdo.manager_type_prefix().is_some())
}

/// Whether an unread bare assignment declares a local at all.
///
/// For an ordinary name it always does, unless a declaration or an implicit
/// member already owns it. For a metadata-collection name the answer belongs to
/// INFERENCE and nowhere else: assigning to a Global-context property declares
/// nothing (the platform refuses the write, and `GlobalPropertyNotWritable`
/// reports it), but a non-writable owner — a same-named method, a common module
/// known from the configuration OR merely from the source root — takes the name
/// first, and then the assignment does declare a real local.
///
/// Answering that here a second time is what produced three separate defects:
/// the local copy disagreed with inference on config-less common modules, on
/// methods, and on the method-versus-attribute resolution order. Inference is
/// consulted only for collection names, so ordinary bodies pay nothing.
fn declares_a_local(
    name_lower: &str,
    body: hir::DefWithBodyId,
    declared_vars: &FxHashSet<String>,
    skip_attr_names: &FxHashSet<String>,
    ctx: &DiagnosticsContext,
) -> bool {
    if names_a_manager_collection(name_lower) {
        // A syntactic binding — parameter, `Перем`, loop variable — owns the name
        // outright, and its own dead-store check already covers it. Inference
        // keeps writes to such bindings in the same table as implicit locals, so
        // that table answers "does a local by this name exist", not "did THIS
        // write declare one"; the declaration is what tells the two apart.
        return !declared_vars.contains(name_lower)
            && ctx
                .infer()
                .implicit_locals_by_body
                .get(&body)
                .is_some_and(|locals| locals.contains_key(name_lower));
    }
    !declared_vars.contains(name_lower) && !skip_attr_names.contains(name_lower)
}

fn check_method(
    local_id: hir::MethodKey,
    body: &hir::Body,
    module_bodies: &hir::ModuleBodies,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
    skip_attr_names: &FxHashSet<String>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let source_map = match module_bodies.source_map(local_id) {
        Some(sm) => sm,
        None => return diagnostics,
    };

    let read_vars = collect_read_var_names(body);

    // A `Для Сч = … По …` counter is syntactically mandatory and cannot simply
    // be "removed", so a project may opt out of reporting unused numeric loop
    // counters. Defaults to reporting them (parity with bsl-language-server).
    let analyze_for_counters = ctx.config.get_bool(code, "analyzeForLoopVariables").unwrap_or(true);

    let mut declared_vars = rustc_hash::FxHashSet::default();

    for param_id in body.params() {
        let binding = body.binding(param_id);
        declared_vars.insert(binding.name.as_str().fold_lower());
    }

    // Walk the full statement arena, not just top-level statements: `Перем`
    // declarations, loop counters and bare-assignment locals nested inside
    // `Если`/loops/`Попытка` are just as much method locals and must be checked
    // for being unused. The read set already spans the whole body.
    for (_, stmt) in body.stmts_iter() {
        if let hir::Stmt::VarDecl { bindings } = stmt {
            for &binding_id in bindings.iter() {
                let binding_id_opaque = BindingId::from_idx(binding_id);
                let binding = body.binding(binding_id_opaque);
                declared_vars.insert(binding.name.as_str().fold_lower());

                if !read_vars.contains(&binding.name.as_str().fold_lower()) {
                    if let Some(range) = source_map.binding_range(binding_id_opaque) {
                        diagnostics.push(create_diagnostic(
                            binding.name.as_str(),
                            range,
                            code,
                            ctx,
                        ));
                    }
                }
            }
        }
    }

    for (_, stmt) in body.stmts_iter() {
        if let hir::Stmt::For { var, .. } = stmt {
            let var_opaque = BindingId::from_idx(*var);
            let binding = body.binding(var_opaque);
            declared_vars.insert(binding.name.as_str().fold_lower());

            if analyze_for_counters && !read_vars.contains(&binding.name.as_str().fold_lower()) {
                if let Some(range) = source_map.binding_range(var_opaque) {
                    diagnostics.push(create_diagnostic(binding.name.as_str(), range, code, ctx));
                }
            }
        }
    }

    for (_, stmt) in body.stmts_iter() {
        if let hir::Stmt::ForEach { var, .. } = stmt {
            let var_opaque = BindingId::from_idx(*var);
            let binding = body.binding(var_opaque);
            declared_vars.insert(binding.name.as_str().fold_lower());

            if !read_vars.contains(&binding.name.as_str().fold_lower()) {
                if let Some(range) = source_map.binding_range(var_opaque) {
                    diagnostics.push(create_diagnostic(binding.name.as_str(), range, code, ctx));
                }
            }
        }
    }

    let mut implicit_vars: rustc_hash::FxHashMap<String, (String, ide_db::TextRange)> =
        rustc_hash::FxHashMap::default();

    for (_, stmt) in body.stmts_iter() {
        if let hir::Stmt::Assign { target, .. } = stmt {
            let target_opaque = hir::ExprId::from_idx(*target);
            if let hir::Expr::Path(name) = body.expr(target_opaque) {
                let lowercase_name = name.as_str().fold_lower();

                if declares_a_local(
                    &lowercase_name,
                    hir::DefWithBodyId::Method(local_id),
                    &declared_vars,
                    skip_attr_names,
                    ctx,
                ) {
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        implicit_vars.entry(lowercase_name)
                    {
                        if let Some(range) = source_map.expr_range(target_opaque) {
                            e.insert((name.as_str().to_string(), range));
                        }
                    }
                }
            }
        }
    }

    for (lowercase_name, (original_name, range)) in implicit_vars {
        if !read_vars.contains(&lowercase_name) {
            diagnostics.push(create_diagnostic(&original_name, range, code, ctx));
        }
    }

    diagnostics
}

fn check_module_level_code(
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
    skip_attr_names: &FxHashSet<String>,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let module_bodies = ctx.module_bodies();

    let lower_result = match module_bodies.module_code_result() {
        Some(result) => result,
        None => return diagnostics,
    };

    let body = lower_result.body();
    let source_map = lower_result.source_map();

    let read_vars = collect_read_var_names(body);

    let mut implicit_vars: rustc_hash::FxHashMap<String, (String, ide_db::TextRange)> =
        rustc_hash::FxHashMap::default();

    for stmt_id in body.body_stmts() {
        if let hir::Stmt::Assign { target, .. } = body.stmt(stmt_id) {
            let target_opaque = hir::ExprId::from_idx(*target);
            if let hir::Expr::Path(name) = body.expr(target_opaque) {
                let lowercase_name = name.as_str().fold_lower();

                if !declares_a_local(
                    &lowercase_name,
                    hir::DefWithBodyId::ModuleCode,
                    &FxHashSet::default(),
                    skip_attr_names,
                    ctx,
                ) {
                    continue;
                }

                if let std::collections::hash_map::Entry::Vacant(e) =
                    implicit_vars.entry(lowercase_name)
                {
                    if let Some(range) = source_map.expr_range(target_opaque) {
                        e.insert((name.as_str().to_string(), range));
                    }
                }
            }
        }
    }

    for (lowercase_name, (original_name, range)) in implicit_vars {
        if !read_vars.contains(&lowercase_name) {
            diagnostics.push(create_diagnostic(&original_name, range, code, ctx));
        }
    }

    diagnostics
}

fn check_module_var_declarations(
    module_bodies: &hir::ModuleBodies,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
) -> Vec<Diagnostic> {
    let module_vars = module_bodies.module_vars();
    if module_vars.is_empty() {
        return Vec::new();
    }

    let mut all_referenced_externals: rustc_hash::FxHashSet<NormName> =
        rustc_hash::FxHashSet::default();

    for (_local_id, lower_result) in module_bodies.iter_lower_results() {
        all_referenced_externals.extend(lower_result.referenced_externals().iter().copied());
    }
    if let Some(module_code_result) = module_bodies.module_code_result() {
        all_referenced_externals.extend(module_code_result.referenced_externals().iter().copied());
    }

    let mut diagnostics = Vec::new();
    for var in module_vars {
        if var.is_export {
            continue;
        }
        let key = NormName::intern(&var.name);
        if !all_referenced_externals.contains(&key) {
            diagnostics.push(create_diagnostic(&var.name, var.range, code, ctx));
        }
    }

    diagnostics
}

fn create_diagnostic(
    name: &str,
    range: TextRange,
    code: DiagnosticCode,
    ctx: &DiagnosticsContext,
) -> Diagnostic {
    Diagnostic {
        code,
        message: format!("Удалите неиспользуемую переменную {}", name),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    }
}

#[cfg(test)]
mod tests {
    /// Неизменяемый владелец забирает имя раньше коллекции, поэтому присваивание
    /// поверх него создаёт настоящую локаль. Вердикт даёт инференс — здесь его
    /// не дублируют, иначе копия расходится с ним на каждом виде владельца.
    #[test]
    fn a_non_writable_owner_makes_the_assignment_a_real_local() {
        let code = "Процедура Справочники()\nКонецПроцедуры\n\nПроцедура Тест()\n    \
                    Справочники = 1;\nКонецПроцедуры\n";
        let diags = crate::test_utils::check_hir_diagnostic(code);
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count(),
            1,
            "the same-named method holds the name"
        );
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::GlobalPropertyNotWritable).count(),
            0,
            "and the write is therefore not a write to the collection"
        );
    }

    /// Тот же владелец, известный только по пути в source root: конфигурации нет,
    /// общий модуль находится через индекс модулей. Инференс это учитывает, и
    /// локальная копия вердикта — не учитывала.
    #[test]
    fn a_config_less_common_module_owner_also_makes_it_a_real_local() {
        let fixture = r#"
//- /CommonModules/Справочники/Ext/Module.bsl
Процедура Метод() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Справочники = 1;
КонецПроцедуры
"#;
        let diags = crate::test_utils::check_hir_diagnostic_with_fixtures(fixture);
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count(),
            1,
            "a common module known only from the source root holds the name too"
        );
    }

    /// Имя коллекции может быть занято НЕИЗМЕНЯЕМЫМ объявленным владельцем —
    /// одноимённым методом модуля или общим модулем. Писать в них нельзя, поэтому
    /// присваивание создаёт настоящую неявную локаль, и её неиспользование —
    /// законная находка. Фильтр по одному написанию имени глушил бы и её.
    #[test]
    fn assignment_over_a_same_named_method_is_still_an_unused_local() {
        let code = "Процедура Справочники()\nКонецПроцедуры\n\nПроцедура Тест()\n    \
                    Справочники = 1;\nКонецПроцедуры\n";
        let diags = crate::test_utils::check_hir_diagnostic(code);
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count(),
            1,
            "the method holds the name, so the assignment declares a real local"
        );
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::GlobalPropertyNotWritable).count(),
            0,
            "and the write is not a write to the collection"
        );
    }

    /// Присваивание имени коллекции метаданных локаль не объявляет: имя
    /// принадлежит свойству глобального контекста, платформа запись отвергает.
    /// Поэтому непрочитанное присваивание ему — не мёртвая запись, а нарушение,
    /// о котором сообщает `GlobalPropertyNotWritable`.
    #[test]
    fn assignment_to_a_collection_name_is_not_an_unused_local() {
        for code in [
            "Процедура Тест()\n    Справочники = Новый Структура;\nКонецПроцедуры\n",
            "Процедура Тест()\n    Catalogs = Новый Структура;\nКонецПроцедуры\n",
            "Справочники = Новый Структура;\n",
        ] {
            let diags = crate::test_utils::check_hir_diagnostic(code);
            let unused: Vec<_> =
                diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();
            assert!(unused.is_empty(), "no local is declared here:\n{code}\ngot {unused:?}");
        }
    }

    /// Уже объявленное имя коллекции локаль присваиванием не создаёт: параметр,
    /// `Перем` и счётчик цикла связывают имя раньше, и запись поверх них — просто
    /// запись в известную переменную. Инференс держит их в той же таблице, что и
    /// неявные локали, поэтому один её просмотр вердиктом об объявлении не является.
    #[test]
    fn an_already_declared_collection_name_is_not_declared_twice() {
        let param = "Процедура Тест(Справочники)\n    Справочники = 1;\nКонецПроцедуры\n";
        let diags = crate::test_utils::check_hir_diagnostic(param);
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count(),
            0,
            "a parameter is not a dead store at its assignment:\n{diags:#?}"
        );

        let var_decl =
            "Процедура Тест()\n    Перем Справочники;\n    Справочники = 1;\nКонецПроцедуры\n";
        let diags = crate::test_utils::check_hir_diagnostic(var_decl);
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count(),
            1,
            "the unread declaration is reported once, not once more at the write:\n{diags:#?}"
        );

        let loop_var =
            "Процедура Тест(Коллекция)\n    Для Каждого Справочники Из Коллекция Цикл\n        \
                        Справочники = 1;\n    КонецЦикла;\nКонецПроцедуры\n";
        let diags = crate::test_utils::check_hir_diagnostic(loop_var);
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count(),
            1,
            "the loop variable is reported once, not once more at the write:\n{diags:#?}"
        );
    }

    /// Диагностики модуля расширения, спаренного с базовым: обе стороны лежат по
    /// одному пути в своих корнях, поэтому слияние расширения включается целиком.
    fn extension_pair_diagnostics(base_text: &str, ext_text: &str) -> Vec<crate::Diagnostic> {
        use crate::DiagnosticsConfig;
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use vfs::{FileId, FileSet, VfsPath};

        let temp = tempfile::tempdir().unwrap();
        let main_root = temp.path().join("src/cf");
        let ext_root = temp.path().join("src/cfe/X");
        std::fs::create_dir_all(&main_root).unwrap();
        std::fs::create_dir_all(&ext_root).unwrap();

        let mut db = RootDatabaseImpl::new();
        db.set_all_config_paths(vec![
            (None, main_root.clone()),
            (Some("X".to_string()), ext_root.clone()),
        ]);

        let main_file = FileId(0);
        let ext_file = FileId(1);
        let mut file_set = FileSet::new();
        let main_path = main_root.join("CommonModules/М/Ext/Module.bsl");
        let ext_path = ext_root.join("CommonModules/М/Ext/Module.bsl");
        file_set.insert(main_file, VfsPath::new(main_path.to_string_lossy().as_ref()));
        file_set.insert(ext_file, VfsPath::new(ext_path.to_string_lossy().as_ref()));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
        db.set_file_source_root(main_file, SourceRootId(0));
        db.set_file_source_root(ext_file, SourceRootId(0));

        db.set_file_text(main_file, base_text);
        db.set_file_text(ext_file, ext_text);
        assert!(
            ide_db::weaving_target(&db, ext_file).is_some(),
            "fixture must actually pair the extension to its base"
        );

        crate::file_diagnostics(&db, ext_file, &DiagnosticsConfig::all_enabled())
    }

    /// Владельца имени может дать только базовый модуль пары расширения. Тогда
    /// вердикт о локали меняется после weaving, и диагностика обязана считаться
    /// заново: standalone-проход видит имя коллекции и локали не находит.
    #[test]
    fn a_base_module_owner_is_seen_through_the_extension_merge() {
        let diags = extension_pair_diagnostics(
            "Процедура Справочники() Экспорт\nКонецПроцедуры\n\nПроцедура Цель() Экспорт\nКонецПроцедуры\n",
            "&Вместо(\"Цель\")\nПроцедура Расш_Цель()\n\tСправочники = 1;\nКонецПроцедуры\n",
        );
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count(),
            1,
            "the base method holds the name, so the write declares a dead local:\n{diags:#?}"
        );
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::GlobalPropertyNotWritable).count(),
            0,
            "and it is therefore not a write to the collection:\n{diags:#?}"
        );
    }

    /// Код `#Вставка` принадлежит effective-проходу, и мёртвая запись внутри него
    /// обязана дожить до конца конвейера — обычная, ничего не знающая об именах
    /// коллекций.
    #[test]
    fn a_dead_store_inside_a_change_and_validate_insert_survives() {
        let diags = extension_pair_diagnostics(
            "Процедура Цель() Экспорт\nКонецПроцедуры\n",
            "&ИзменениеИКонтроль(\"Цель\")\nПроцедура Цель()\n#Вставка\n\tМертвая = 1;\n#КонецВставки\nКонецПроцедуры\n",
        );
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count(),
            1,
            "the insert's own dead store belongs to the extension author:\n{diags:#?}"
        );
    }

    /// Синтаксическую находку вытесняет только тот вердикт, который дожил до конца.
    /// Базовый модуль отменяет `GlobalPropertyNotWritable`, и вытесненная им
    /// `SelfAssign` обязана вернуться.
    #[test]
    fn a_superseded_self_assign_returns_when_the_base_cancels_the_verdict() {
        let diags = extension_pair_diagnostics(
            "Процедура Справочники() Экспорт\nКонецПроцедуры\n\nПроцедура Цель() Экспорт\nКонецПроцедуры\n",
            "&Вместо(\"Цель\")\nПроцедура Расш_Цель()\n\tСправочники = Справочники;\nКонецПроцедуры\n",
        );
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::GlobalPropertyNotWritable).count(),
            0,
            "the base method owns the name:\n{diags:#?}"
        );
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::SelfAssign).count(),
            1,
            "nothing supersedes the self-assignment any more:\n{diags:#?}"
        );
    }

    /// Контроль: обычное имя по-прежнему объявляет локаль, иначе проверка выше
    /// не способна упасть.
    #[test]
    fn assignment_to_an_ordinary_name_is_still_an_unused_local() {
        for code in [
            "Процедура Тест()\n    МояПеременная = Новый Структура;\nКонецПроцедуры\n",
            "МояПеременная = Новый Структура;\n",
        ] {
            let diags = crate::test_utils::check_hir_diagnostic(code);
            let unused =
                diags.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).count();
            assert_eq!(unused, 1, "an ordinary name is a real dead store:\n{code}");
        }
    }

    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    #[test]
    fn test_unused_var_in_procedure() {
        let code = r#"Процедура Тест()
    Перем НеИспользуется;
    Сообщить("Привет");
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 2:11..2:25
              message: Удалите неиспользуемую переменную НеИспользуется
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_used_var_no_diagnostic() {
        let code = r#"Процедура Тест()
    Перем Сообщение;
    Сообщение = "Привет";
    Сообщить(Сообщение);
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_unused_loop_variable() {
        let code = r#"Процедура Тест()
    Для Индекс = 1 По 10 Цикл
        Сообщить("Итерация");
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 2:9..2:15
              message: Удалите неиспользуемую переменную Индекс
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_used_loop_variable() {
        let code = r#"Процедура Тест()
    Для Индекс = 1 По 10 Цикл
        Сообщить(Индекс);
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_unused_foreach_variable() {
        let code = r#"Процедура Тест()
    Для Каждого Элемент Из Коллекция Цикл
        Сообщить("Итерация");
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 2:17..2:24
              message: Удалите неиспользуемую переменную Элемент
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_multiple_unused_vars() {
        let code = r#"Процедура Тест()
    Перем А, Б, В;
    Сообщить(Б);
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 2:11..2:12
              message: Удалите неиспользуемую переменную А
              severity: Warning
            UnusedLocalVariable @ 2:17..2:18
              message: Удалите неиспользуемую переменную В
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_case_insensitive_usage() {
        let code = r#"Процедура Тест()
    Перем Переменная;
    ПЕРЕМЕННАЯ = 10;
    Сообщить(переменная);
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_assigned_but_never_read() {
        let code = r#"Процедура Тест()
    Перем ТолькоПрисвоение;
    ТолькоПрисвоение = 10;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 2:11..2:27
              message: Удалите неиспользуемую переменную ТолькоПрисвоение
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_assigned_and_read() {
        let code = r#"Процедура Тест()
    Перем Значение;
    Значение = 10;
    Сообщить(Значение);
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_multiple_assignments_no_read() {
        let code = r#"Процедура Тест()
    Перем Результат;
    Результат = ПервоеДействие();
    Результат = ВтороеДействие();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 2:11..2:20
              message: Удалите неиспользуемую переменную Результат
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_field_assignment_base_is_read() {
        let code = r#"Процедура Тест()
    Перем Структура;
    Структура = Новый Структура;
    Структура.Поле = 10;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_index_assignment_base_is_read() {
        let code = r#"Процедура Тест()
    Перем Массив;
    Массив = Новый Массив;
    Массив[0] = 10;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_fixture_local_variables_in_function() {
        let code = r#"Функция Вторая()
    Перем ЛокальнаяБезИспользования, ТолькоСПрисвоениемЗначения, ЛокальнаяСИспользованием;

    ЛокальнаяСИспользованием = 40;
    ТолькоСПрисвоениемЗначения = ВыполнитьДействие(ЛокальнаяСИспользованием);
    ВПроцедуреИспользуемая = Проверка();
    ВПроцедуреНеИспользуемая = Проверка();

    Если ВПроцедуреИспользуемая = Истина Тогда

       ТолькоСПрисвоениемЗначения = 39;

    КонецЕсли;

    ПеременнаяОбъектСИспользованием = Обработки.Проверка.Создать();
    ПеременнаяОбъектСИспользованием.Выполнить();

    ВПроцедуреИспользуемая2 = Новый Файл(ОбъединитьПути(".", "test_versions.mxl"));
    Ожидаем.Что(ВПроцедуреИспользуемая2.Существует(), "Файл отчета не был создан").ЭтоИстина();

КонецФункции"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 2:11..2:36
              message: Удалите неиспользуемую переменную ЛокальнаяБезИспользования
              severity: Warning
            UnusedLocalVariable @ 2:38..2:64
              message: Удалите неиспользуемую переменную ТолькоСПрисвоениемЗначения
              severity: Warning
            UnusedLocalVariable @ 7:5..7:29
              message: Удалите неиспользуемую переменную ВПроцедуреНеИспользуемая
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_module_level_unused_variable() {
        let code = r#"Перем НеИспользуемая;

Процедура Тест()
    Сообщить("Привет");
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 1:7..1:21
              message: Удалите неиспользуемую переменную НеИспользуемая
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_module_level_export_variable_not_flagged() {
        let code = r#"Перем ЭкспортнаяПеременная Экспорт;

Процедура Тест()
    Сообщить("Привет");
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_module_level_used_variable() {
        let code = r#"Перем ИспользуемаяПеременная;

Процедура Тест()
    Сообщить(ИспользуемаяПеременная);
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_module_level_code_unused_variable() {
        let code = r#"НеИспользуемаяВМодуле = 30;
ИспользуемаяВМодуле = 40;
Сообщить(ИспользуемаяВМодуле);"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 1:1..1:22
              message: Удалите неиспользуемую переменную НеИспользуемаяВМодуле
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_var_used_in_while_condition() {
        let code = r#"Процедура ЗапускПроцессовДО()
    ЕстьЗадания = Истина;
    Пока ЕстьЗадания Цикл
        ВыполнитьДействие();
        ЕстьЗадания = ПроверитьУсловие();
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    /// A variable read and then reassigned inside the same conditional branch is
    /// used (the read observes the value defined before the branch). Block-level
    /// liveness used to mark it dead because the branch block ends in the kill.
    #[test]
    fn test_var_read_then_reassigned_in_branch_is_used() {
        let code = r#"Процедура Тест()
    ПредыдущееЗначение = Неопределено;
    Если Условие() Тогда
        Сообщить(ПредыдущееЗначение);
        ПредыдущееЗначение = 5;
    КонецЕсли;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    /// Same pattern inside a loop body: the value initialised before the loop is
    /// read on the first iteration before being reassigned for the next one.
    #[test]
    fn test_var_read_then_reassigned_in_loop_is_used() {
        let code = r#"Процедура Тест()
    Предыдущее = Неопределено;
    Для Сч = 1 По 3 Цикл
        Сообщить(Предыдущее);
        Предыдущее = Сч;
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    /// A resource created, consumed through method calls, then cleared with a
    /// trailing `= Неопределено` kill — the variable is used despite the block
    /// ending in a dead store.
    #[test]
    fn test_var_used_via_method_before_trailing_kill() {
        let code = r#"Процедура Тест()
    ЗаписьТекста = Новый ЗаписьТекста("файл.txt");
    ЗаписьТекста.Записать("привет");
    ЗаписьТекста.Закрыть();
    ЗаписьТекста = Неопределено;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    /// `analyzeForLoopVariables = false` suppresses unused `Для` counters
    /// (which cannot be removed) while leaving every other unused-local report
    /// intact; the default still reports them.
    #[test]
    fn test_unused_for_counter_respects_opt_out() {
        use crate::test_utils::check_ast_diagnostic_with_config;
        use crate::DiagnosticsConfig;

        let code = r#"Процедура Тест()
    Для Сч = 1 По 3 Цикл
        Сообщить("итерация");
    КонецЦикла;
КонецПроцедуры"#;

        let default_diags = check_ast_diagnostic_with_config(
            code,
            DiagnosticsConfig::all_enabled(),
            crate::diagnostics,
        );
        assert!(
            default_diags
                .iter()
                .any(|d| d.code == DiagnosticCode::UnusedLocalVariable && d.message.contains("Сч")),
            "an unused Для counter must be reported by default"
        );

        let mut config = DiagnosticsConfig::all_enabled();
        config.parameters.insert(
            DiagnosticCode::UnusedLocalVariable,
            serde_json::json!({ "analyzeForLoopVariables": false }),
        );
        let opt_out_diags = check_ast_diagnostic_with_config(code, config, crate::diagnostics);
        assert!(
            !opt_out_diags.iter().any(|d| d.code == DiagnosticCode::UnusedLocalVariable),
            "analyzeForLoopVariables=false must suppress the unused Для counter"
        );
    }

    /// A bare-assignment local that is first written inside a nested block
    /// (`Если`/loop/`Попытка`) and never read is still a dead local — the
    /// population walk must descend into nested statements, not only top-level.
    #[test]
    fn test_unused_variable_nested_in_branch_is_flagged() {
        let code = r#"Процедура Тест()
    Если Условие() Тогда
        ВременноеЗначение = ВычислитьЧтоТо();
    КонецЕсли;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
                UnusedLocalVariable @ 3:9..3:26
                  message: Удалите неиспользуемую переменную ВременноеЗначение
                  severity: Warning"#]],
        );
    }

    /// A module `Перем` assigned inside one procedure and read in another is a
    /// module variable, not a dead local — the assignment must not be flagged.
    /// Its unused-ness is owned by the module-variable check, which sees reads
    /// across all bodies.
    #[test]
    fn test_module_var_assigned_in_procedure_is_not_local() {
        let code = r#"Перем КонтекстКлиент;

Процедура Инициализировать() Экспорт
    КонтекстКлиент = ПолучитьКонтекст();
КонецПроцедуры

Процедура Использовать() Экспорт
    КонтекстКлиент.Выполнить();
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_detects_unused_local_variables_in_fixture() {
        let code = r#"&НаКлиенте
Перем ПеременнаяМодуляНеИспользуемая; // Тут ошибка

&НаСервере
Перем ПеременнаяМодуляНеИспользуемая; // Тут без ошибок

Перем ПеременнаяМодуляНеИспользуемаяЭкспортная Экспорт; // Тут думаю ошибка не нужно, возможно ради поддержания интефейса
Перем ПеременнаяМодуляИспользуемая; // Тут без ошибок
Перем ПеременнаяМодуляИспользуемаяЭкспортная Экспорт; // Тут без ошибок

Функция Первая()

    ПеременнаяМодуляИспользуемая = ДействиеСРезультатомЧисло();
    ДействиеСПараметром(ПеременнаяМодуляИспользуемая);
    ДействиеСПараметром2(ПеременнаяМодуляИспользуемаяЭкспортная);

КонецФункции

Функция Вторая()
    Перем ЛокальнаяБезИспользования, ТолькоСПрисвоениемЗначения, ЛокальнаяСИспользованием;

    ЛокальнаяСИспользованием = 40;
    ТолькоСПрисвоениемЗначения = ВыполнитьДействие(ЛокальнаяСИспользованием);
    ВПроцедуреИспользуемая = Проверка();
    ВПроцедуреНеИспользуемая = Проверка();

    Если ВПроцедуреИспользуемая = Истина Тогда

       ТолькоСПрисвоениемЗначения = 39;

    КонецЕсли;

    ПеременнаяОбъектСИспользованием = Обработки.Проверка.Создать();
    ПеременнаяОбъектСИспользованием.Выполнить();

    ВПроцедуреИспользуемая2 = Новый Файл(ОбъединитьПути(".", "test_versions.mxl"));
    Ожидаем.Что(ВПроцедуреИспользуемая2.Существует(), "Файл отчета не был создан").ЭтоИстина();

КонецФункции

Функция Третья(ЭтоПараметр)

    ЭтоПараметр = Новый Массив();

    НоваяСтрока                = ГруппаДоступа.ВидыДоступа.Добавить();
    НоваяСтрока.ВидДоступа     = СтрокаВидаДоступа.ВидДоступа;
    НоваяСтрока.ДоступРазрешен = СтрокаВидаДоступа.ДоступРазрешен;

КонецФункции

Процедура ЗаполнитьСвойстваОбъектаОбъектнойМоделиCOMАдминистратораПоОписанию(Объект, Знач Описание, Знач Словарь)

	Для Каждого ФрагментСловаря Из Словарь Цикл

		ИмяСвойства = ФрагментСловаря.Значение;

		ЗначениеСвойства = Описание[ФрагментСловаря.Ключ];

		Объект[ИмяСвойства] = ЗначениеСвойства;

	КонецЦикла;

КонецПроцедуры

Процедура ВывестиШапкуПоВерсии(ТЧОтчета, Знач Текст, Знач НомерСтроки, Знач НомерКолонки)

	Если Не ПустаяСтрока(Текст) Тогда

		ТЧОтчета.Область("C"+Строка(НомерКолонки)).ШиринаКолонки = 50;

		Регион = "R" + Формат(НомерСтроки, "ЧГ=0") + "C" + Формат(НомерКолонки, "ЧГ=0");
		ТЧОтчета.Область(Регион).Текст = Текст;
		ТЧОтчета.Область(Регион).ЦветФона = ЦветаСтиля.ТекстЗапрещеннойЯчейкиЦвет;
		ТЧОтчета.Область(Регион).Шрифт = Новый Шрифт(, 8, Истина, , , );
		ТЧОтчета.Область(Регион).ГраницаСверху = Новый Линия(ТипЛинииЯчейкиТабличногоДокумента.Сплошная);
		ТЧОтчета.Область(Регион).ГраницаСнизу  = Новый Линия(ТипЛинииЯчейкиТабличногоДокумента.Сплошная);
		ТЧОтчета.Область(Регион).ГраницаСлева  = Новый Линия(ТипЛинииЯчейкиТабличногоДокумента.Сплошная);
		ТЧОтчета.Область(Регион).ГраницаСправа = Новый Линия(ТипЛинииЯчейкиТабличногоДокумента.Сплошная);

	КонецЕсли;

КонецПроцедуры

ВнеПроцедурНеИспользуемая = 30;
ВнеПроцедурИспользуемая = 40;
ДействиеСПараметром(ВнеПроцедурИспользуемая);

Комиссия = Источник.Комиссия;

Если Истина Тогда

    Комментарий = "Тест1" + Комиссия;

Иначе

    Комментарий = "Тест2" + Комиссия;

КонецЕсли;

Сообщить(Комментарий);
"#;

        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::UnusedLocalVariable,
            expect![[r#"
            UnusedLocalVariable @ 2:7..2:37
              message: Удалите неиспользуемую переменную ПеременнаяМодуляНеИспользуемая
              severity: Warning
            UnusedLocalVariable @ 20:11..20:36
              message: Удалите неиспользуемую переменную ЛокальнаяБезИспользования
              severity: Warning
            UnusedLocalVariable @ 20:38..20:64
              message: Удалите неиспользуемую переменную ТолькоСПрисвоениемЗначения
              severity: Warning
            UnusedLocalVariable @ 25:5..25:29
              message: Удалите неиспользуемую переменную ВПроцедуреНеИспользуемая
              severity: Warning
            UnusedLocalVariable @ 84:1..84:26
              message: Удалите неиспользуемую переменную ВнеПроцедурНеИспользуемая
              severity: Warning"#]],
        );
    }

    #[test]
    fn test_foreach_collection_variable_is_used() {
        let code = r#"Процедура ОжидатьЗавершенияВыполненияЗадания(КлючЗадания)
    Отбор = Новый Структура;
    Отбор.Вставить("Ключ", КлючЗадания);
    НайденныеФоновыеЗадания = ФоновыеЗадания.ПолучитьФоновыеЗадания(Отбор);

    Для Каждого ФоновоеЗадание Из НайденныеФоновыеЗадания Цикл
        Если ФоновоеЗадание.Состояние = СостояниеФоновогоЗадания.Активно
            ИЛИ ФоновоеЗадание.Состояние <> СостояниеФоновогоЗадания.Завершено Тогда
                ФоновоеЗадание.ОжидатьЗавершения();
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_for_loop_bound_variable_is_used() {
        let code = r#"Процедура Тест()
    КоличествоКолонок = 4;
    Для Сч = 1 По КоличествоКолонок Цикл
        Сообщить(Сч);
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    #[test]
    fn test_for_loop_from_bound_variable_is_used() {
        let code = r#"Процедура Тест()
    Начало = 1;
    Для Сч = Начало По 10 Цикл
        Сообщить(Сч);
    КонецЦикла;
КонецПроцедуры"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }

    fn make_object_module_metadata(mdo: bsl_metadata::MetadataObject) -> hir::ModuleMetadata {
        hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::ObjectModule,
            execution_context: None,
            common_module: None,
            mdo: Some(std::sync::Arc::new(mdo)),
            register: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: None,
        }
    }

    #[test]
    fn test_object_attribute_not_flagged_in_object_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let mut mdo =
            bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::BusinessProcess, "Исполнение");
        mdo.add_attribute(bsl_metadata::Attribute {
            name: "Дата".to_string(),
            name_en: Some("Date".to_string()),
            attr_type: bsl_metadata::AttributeType::DateTime,
        });
        mdo.add_attribute(bsl_metadata::Attribute {
            name: "Автор".to_string(),
            name_en: None,
            attr_type: bsl_metadata::AttributeType::Unknown,
        });

        let metadata = make_object_module_metadata(mdo);

        let code = r#"Процедура ПриЗаписи(Отказ)
    Дата = ТекущаяДатаСеанса();
    Автор = ПользователиИнформационнойБазы.ТекущийПользователь();
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "Object attributes should not be flagged as unused in ObjectModule, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_tabular_section_not_flagged_in_object_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let mut mdo = bsl_metadata::MetadataObject::new(
            bsl_metadata::MdoType::Document,
            "ПриходнаяНакладная",
        );
        let ts = bsl_metadata::TabularSection::new(uuid::Uuid::nil(), "Товары");
        mdo.add_tabular_section(ts);

        let metadata = make_object_module_metadata(mdo);

        let code = r#"Процедура ПриЗаписи(Отказ)
    Товары = ЭтотОбъект.Товары.Выгрузить();
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "Tabular section name should not be flagged in ObjectModule, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_true_unused_still_flagged_in_object_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let mdo =
            bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::BusinessProcess, "Исполнение");

        let metadata = make_object_module_metadata(mdo);

        let code = r#"Процедура ПриЗаписи(Отказ)
    НеАтрибутОбъекта = 42;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "True unused variable should still be flagged in ObjectModule"
        );
        assert!(unused_diags[0].message.contains("НеАтрибутОбъекта"));
    }

    fn make_form_module_metadata(attribute_names: Vec<&str>) -> hir::ModuleMetadata {
        make_form_module_metadata_of(bsl_metadata::FormType::Managed, attribute_names)
    }

    fn make_form_module_metadata_of(
        form_type: bsl_metadata::FormType,
        attribute_names: Vec<&str>,
    ) -> hir::ModuleMetadata {
        let mut form =
            bsl_metadata::Form::new("ТестоваяФорма".to_string(), form_type, uuid::Uuid::nil());
        form.attributes = attribute_names
            .into_iter()
            .map(|s| bsl_metadata::FormAttribute::new(s, bsl_metadata::AttributeType::Unknown))
            .collect();

        hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::FormModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: None,
            http_service: None,
            web_service: None,
            integration_service: None,
            form: Some(std::sync::Arc::new(form)),
        }
    }

    #[test]
    fn test_form_attribute_not_flagged_in_form_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let metadata =
            make_form_module_metadata(vec!["Замечание", "ТекущееОписание", "ИсправленноеОписание"]);

        let code = r#"&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Замечание = Параметры.Замечание;
    ТекущееОписание = Параметры.ТекущееОписание;
    ИсправленноеОписание = Параметры.Предложение;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "Form attributes should not be flagged as unused in FormModule, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_standard_form_property_not_flagged_in_form_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let metadata = make_form_module_metadata(vec![]);

        let code = r#"&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Заголовок = "Проверка описания — строка " + Параметры.НомерСтроки;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "Standard form property 'Заголовок' should not be flagged as unused in FormModule, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_writable_form_property_not_flagged_in_form_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let metadata = make_form_module_metadata(vec![]);

        // ТекущийЭлемент / КлючУникальности / Доступность are writable managed-form
        // standard properties absent from the legacy hand-maintained subset.
        let code = r#"&НаКлиенте
Процедура УстановитьФокус(Команда)
    ТекущийЭлемент = Элементы.Поле;
    КлючУникальности = ДополнительныеПараметры.Ключ;
    Доступность = Истина;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "Writable standard form properties must not be flagged as unused, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_window_opening_mode_not_flagged_in_form_module() {
        use crate::test_utils::check_metadata_diagnostic;

        // `РежимОткрытияОкна` is a writable managed-form property (v8std #std742),
        // but the help archive files it under its enum type name, so it reaches
        // the base contract only through the curated overlay. Writing it is a
        // form-context side effect, not a dead local.
        let metadata = make_form_module_metadata(vec![]);

        let code = r#"&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    РежимОткрытияОкна = РежимОткрытияОкнаФормы.БлокироватьОкноВладельца;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "РежимОткрытияОкна is a managed-form property, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_form_extension_property_not_flagged_in_form_module() {
        use crate::test_utils::check_metadata_diagnostic;

        // `ВариантМодифицирован` / `ПользовательскиеНастройкиМодифицированы` are
        // standard properties of the report-form extension, absent from the base
        // `ФормаКлиентскогоПриложения` contract. Assigning them writes the form
        // context, so they must not be reported as dead locals.
        let metadata = make_form_module_metadata(vec![]);

        let code = r#"&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    ВариантМодифицирован = Параметры.ВариантМодифицирован;
    ПользовательскиеНастройкиМодифицированы = Параметры.ПользовательскиеНастройкиМодифицированы;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "Report-form extension properties must not be flagged as unused, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_managed_only_base_property_still_flagged_in_ordinary_form() {
        use crate::test_utils::check_metadata_diagnostic;

        // `КлючНазначенияИспользования` is a managed-form-only base property. In
        // an ordinary form it is not part of the contract, so the base selection
        // must not skip it — a dead store keeps being reported. This locks the
        // fix that stopped unioning both form bases together.
        let metadata = make_form_module_metadata_of(bsl_metadata::FormType::Ordinary, vec![]);

        let code = r#"Процедура ПриОткрытии()
    КлючНазначенияИспользования = "тест";
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "a managed-only base property is not part of the ordinary-form contract and must be flagged, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        assert!(unused_diags[0].message.contains("КлючНазначенияИспользования"));
    }

    #[test]
    fn test_field_control_extension_property_still_flagged_in_form_module() {
        use crate::test_utils::check_metadata_diagnostic;

        // `Документ` is a property of the HTML-document *field* extension, not of
        // the form context. It must stay reportable: a dead `Документ = …` local
        // in a form module is a real unused variable, not a form-context write.
        let metadata = make_form_module_metadata(vec![]);

        let code = r#"&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Документ = 42;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "a field-control extension property name is an ordinary local and must be flagged"
        );
        assert!(unused_diags[0].message.contains("Документ"));
    }

    #[test]
    fn test_true_unused_still_flagged_in_form_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let metadata = make_form_module_metadata(vec!["Замечание"]);

        let code = r#"&НаСервере
Процедура ПриСозданииНаСервере(Отказ, СтандартнаяОбработка)
    Замечание = Параметры.Замечание;
    НеРеквизитФормы = 42;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "True unused variable should still be flagged in FormModule"
        );
        assert!(unused_diags[0].message.contains("НеРеквизитФормы"));
    }

    #[test]
    fn test_form_attribute_still_flagged_in_common_module() {
        use crate::test_utils::{check_metadata_diagnostic, make_non_common_module_metadata};

        let metadata = make_non_common_module_metadata(bsl_metadata::ModuleType::CommonModule);

        let code = r#"Процедура Тест()
    Замечание = "тест";
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "Form attribute name should be flagged in CommonModule (not a form context)"
        );
        assert!(unused_diags[0].message.contains("Замечание"));
    }

    #[test]
    fn test_attribute_name_still_flagged_in_common_module() {
        use crate::test_utils::{check_metadata_diagnostic, make_non_common_module_metadata};

        let metadata = make_non_common_module_metadata(bsl_metadata::ModuleType::CommonModule);

        let code = r#"Процедура Тест()
    Дата = ТекущаяДатаСеанса();
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "Same name should be flagged in CommonModule (not an object attribute)"
        );
        assert!(unused_diags[0].message.contains("Дата"));
    }

    fn make_record_set_module_metadata() -> hir::ModuleMetadata {
        use bsl_metadata::{dimension::DimensionBuilder, register::RegisterResource};
        let register = bsl_metadata::Register::builder()
            .name("Остатки")
            .mdo_type(bsl_metadata::MdoType::AccumulationRegister)
            .dimensions(vec![DimensionBuilder::default().name("Склад").build()])
            .resources(vec![RegisterResource::new(Default::default(), "Количество")])
            .build();

        hir::ModuleMetadata {
            module_type: bsl_metadata::ModuleType::RecordSetModule,
            execution_context: None,
            common_module: None,
            mdo: None,
            register: Some(std::sync::Arc::new(register)),
            http_service: None,
            web_service: None,
            integration_service: None,
            form: None,
        }
    }

    #[test]
    fn test_register_fields_not_flagged_in_record_set_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let metadata = make_record_set_module_metadata();

        let code = r#"Процедура ПередЗаписью(Отказ, Замещение)
    Склад = Справочники.Склады.ОсновнойСклад();
    Количество = 0;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "Register dimensions/resources should not be flagged in RecordSetModule, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_true_unused_still_flagged_in_record_set_module() {
        use crate::test_utils::check_metadata_diagnostic;

        let metadata = make_record_set_module_metadata();

        let code = r#"Процедура ПередЗаписью(Отказ, Замещение)
    НеПолеРегистра = 42;
КонецПроцедуры"#;

        let diagnostics = check_metadata_diagnostic(metadata, code, |_meta, ctx| super::check(ctx));
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            1,
            "True unused variable should still be flagged in RecordSetModule"
        );
        assert!(unused_diags[0].message.contains("НеПолеРегистра"));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn test_record_set_platform_property_not_flagged_with_salsa() {
        use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
        use ide_db::RootDatabaseImpl;
        use std::path::PathBuf;
        use vfs::{FileId, FileSet, VfsPath};

        let fixtures_dir =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");

        // Assigning the platform record-set property is a side effect (managed
        // lock on write), not a local variable.
        let code = r#"Процедура ПередЗаписью(Отказ, Замещение)
    БлокироватьДляИзменения = Истина;
КонецПроцедуры"#;

        let mut db = RootDatabaseImpl::new();
        let workspace_root = PathBuf::from(fixtures_dir);

        let mut file_set = FileSet::default();
        let file_id = FileId(0);
        let module_path = VfsPath::new(format!(
            "{}/AccumulationRegisters/РегистрНакопления1/Ext/RecordSetModule.bsl",
            fixtures_dir
        ));
        file_set.insert(file_id, module_path);

        let source_root_id = SourceRootId(0);
        db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
        db.set_file_source_root(file_id, source_root_id);
        db.set_file_text(file_id, code);

        let configuration_path_input = ide_db::metadata::ConfigurationPathInput::new(
            &db,
            workspace_root.to_string_lossy().to_string(),
            0,
        );

        let provider = ide_db::SalsaProvider::new(&db, Some(configuration_path_input));
        let config = crate::DiagnosticsConfig::all_enabled();
        let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);

        let diagnostics = super::check(&ctx);
        let unused_diags: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedLocalVariable).collect();

        assert_eq!(
            unused_diags.len(),
            0,
            "БлокироватьДляИзменения is a platform record-set property, got: {:?}",
            unused_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_var_used_in_try_except_not_flagged() {
        let code = r#"Функция Тест()
    Перем Результат;
    Результат = Новый Структура;
    Попытка
        Результат.Вставить("success", Истина);
    Исключение
        Результат.Вставить("success", Ложь);
    КонецПопытки;
    Возврат Результат;
КонецФункции"#;

        check_diagnostics_snapshot_for(code, DiagnosticCode::UnusedLocalVariable, expect![[r#""#]]);
    }
}
