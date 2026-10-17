use crate::{
    item_tree::{
        Annotation, AnnotationKind, Function, ItemTree, MethodKeys, ModItem, Param, Procedure,
        Variable,
    },
    Name,
};
use base_db::RootQueryDb;
use std::sync::Arc;
use syntax::{
    ast::{self, AstNode},
    SyntaxKind, SyntaxNode,
};
use tracing::{debug, trace};
use vfs::FileId;

pub(super) struct Ctx {
    tree: ItemTree,
    keys: MethodKeys,
}

impl Ctx {
    pub fn lower_file(db: &dyn RootQueryDb, file_id: FileId) -> Arc<ItemTree> {
        let _span = tracing::info_span!("lower_file", ?file_id).entered();

        let parse = db.parse_ref(file_id);
        let file = match ast::SourceFile::cast(parse.syntax_node()) {
            Some(f) => f,
            None => {
                debug!("parse result is not a SourceFile, returning empty ItemTree");
                return Arc::new(ItemTree::default());
            }
        };

        let mut ctx = Ctx { tree: ItemTree::default(), keys: MethodKeys::default() };

        ctx.lower_module_items(&file);

        debug!(
            procedures = ctx.tree.procedures.len(),
            functions = ctx.tree.functions.len(),
            variables = ctx.tree.variables.len(),
            "lowering complete"
        );

        Arc::new(ctx.tree)
    }

    fn lower_module_items(&mut self, file: &ast::SourceFile) {
        for node in file.syntax().descendants() {
            match node.kind() {
                SyntaxKind::PROCEDURE_DEF => {
                    if let Some(proc) = ast::ProcedureDef::cast(node.clone()) {
                        self.lower_procedure(&proc);
                    }
                }
                SyntaxKind::FUNCTION_DEF => {
                    if let Some(func) = ast::FunctionDef::cast(node.clone()) {
                        self.lower_function(&func);
                    }
                }
                SyntaxKind::VAR_DEF if !is_inside_method(&node) => {
                    if let Some(var) = ast::VarDef::cast(node.clone()) {
                        self.lower_variable(&var);
                    }
                }
                SyntaxKind::PRE_IF_DIR if !is_inside_method(&node) => {
                    self.tree.has_module_preproc = true;
                }
                _ => {}
            }
        }
    }

    fn lower_procedure(&mut self, proc: &ast::ProcedureDef) {
        let name_token = proc.name_or_keyword();
        let name = name_token.as_ref().map(|t| Name::new(t.text())).unwrap_or_else(Name::missing);
        let name_range = name_token
            .as_ref()
            .map(|t| t.text_range())
            .unwrap_or_else(|| proc.syntax().text_range());

        let is_export = proc.export_keyword().is_some();
        let params = self.lower_params(proc.param_list());
        let param_list_range = proc.param_list().and_then(|pl| calculate_params_content_range(&pl));
        let annotations = self.lower_annotations(proc.annotations());
        let source_range = proc.syntax().text_range();
        let sig_end = header_end(proc.export_keyword(), proc.param_list(), name_range);

        trace!(name = %name, is_export, "lowering procedure");

        let key = self.keys.next(&name);
        let idx = self.tree.procedures.alloc(Procedure {
            key,
            name,
            is_export,
            params,
            annotations,
            source_range,
            name_range,
            param_list_range,
            sig_end,
        });

        self.tree.top_level.push(ModItem::Procedure(idx));
        self.tree.methods_by_key.insert(key, ModItem::Procedure(idx));
    }

    fn lower_function(&mut self, func: &ast::FunctionDef) {
        let name_token = func.name_or_keyword();
        let name = name_token.as_ref().map(|t| Name::new(t.text())).unwrap_or_else(Name::missing);
        let name_range = name_token
            .as_ref()
            .map(|t| t.text_range())
            .unwrap_or_else(|| func.syntax().text_range());

        let is_export = func.export_keyword().is_some();
        let params = self.lower_params(func.param_list());
        let param_list_range = func.param_list().and_then(|pl| calculate_params_content_range(&pl));
        let annotations = self.lower_annotations(func.annotations());
        let source_range = func.syntax().text_range();
        let sig_end = header_end(func.export_keyword(), func.param_list(), name_range);

        trace!(name = %name, is_export, "lowering function");

        let key = self.keys.next(&name);
        let idx = self.tree.functions.alloc(Function {
            key,
            name,
            is_export,
            params,
            annotations,
            source_range,
            name_range,
            param_list_range,
            sig_end,
        });

        self.tree.top_level.push(ModItem::Function(idx));
        self.tree.methods_by_key.insert(key, ModItem::Function(idx));
    }

    fn lower_variable(&mut self, var: &ast::VarDef) {
        let annotations = self.lower_annotations(var.annotations());
        let source_range = var.syntax().text_range();
        for (name_token, is_export) in var.names_with_export() {
            let name = Name::new(name_token.text());
            let name_range = name_token.text_range();

            trace!(name = %name, is_export, "lowering variable");

            let idx = self.tree.variables.alloc(Variable {
                name,
                is_export,
                annotations: annotations.clone(),
                source_range,
                name_range,
            });

            self.tree.top_level.push(ModItem::Variable(idx));
        }
    }

    fn lower_params(&mut self, param_list: Option<ast::ParamList>) -> Box<[Param]> {
        use syntax::ast::AstNode;

        let Some(param_list) = param_list else {
            return Box::new([]);
        };

        param_list
            .params()
            .map(|p| {
                let name_token = p.name();
                let name =
                    name_token.as_ref().map(|t| Name::new(t.text())).unwrap_or_else(Name::missing);
                let name_range = name_token
                    .as_ref()
                    .map(|t| t.text_range())
                    .unwrap_or_else(|| p.syntax().text_range());

                let is_val = p.val_keyword().is_some();
                let has_default = p.default_value();
                // Capture the default expression's source text so a declaration renders faithfully
                // as `Имя = Неопределено` rather than dropping it. Only a multi-line default is
                // whitespace-collapsed (to keep the declaration one line); a single-line default is
                // taken verbatim so spaces inside a string literal (`= "Иван  Петров"`) survive.
                let default_value = p.default_value_expr().map(|n| {
                    let text = n.text().to_string();
                    let normalized = if text.contains('\n') {
                        text.split_whitespace().collect::<Vec<_>>().join(" ")
                    } else {
                        text.trim().to_string()
                    };
                    smol_str::SmolStr::new(normalized)
                });

                Param { name, is_val, has_default, default_value, name_range }
            })
            .collect()
    }

    fn lower_annotations(
        &mut self,
        annotations: impl Iterator<Item = ast::Annotation>,
    ) -> Box<[Annotation]> {
        annotations
            .filter_map(|ann| {
                let token = ann.kind_token()?;
                let range = ann.syntax().text_range();

                // Написание приводится к одному виду, потому что регистр во
                // встроенном языке не значим, а лексер отдаёт текст как есть.
                // Сравнение по сырому тексту роняло бы `&НАКЛИЕНТЕ` и `&around`
                // в неизвестные: разбор их принимает, а семантики они не
                // получают.
                //
                // Подъём регистра перед спуском — не описка: он складывает
                // регистр по Unicode так же, как `(?i)` в лексере, где `ſ`
                // равна `s`. Один лишь спуск `ſ` не трогает, и слой написаний
                // расходился бы с тем, что лексер уже принял.
                let spelling = token.text().trim_start_matches('&').to_uppercase().to_lowercase();

                let kind = match spelling.as_str() {
                    "наклиенте" | "atclient" => AnnotationKind::AtClient,
                    "насервере" | "atserver" => AnnotationKind::AtServer,
                    "наклиентенасервере" | "atclientatserver" => {
                        AnnotationKind::AtClientAtServer
                    }
                    "наклиентенасерверебезконтекста" | "atclientatservernocontext" => {
                        AnnotationKind::AtClientAtServerNoContext
                    }
                    "насерверебезконтекста" | "atservernocontext" => {
                        AnnotationKind::AtServerNoContext
                    }
                    "перед" | "before" => AnnotationKind::Before,
                    "после" | "after" => AnnotationKind::After,
                    // `instead` источником не задан и принимается разрешением
                    // совместимости: 4.8.2 даёт английское имя `Вместо` как
                    // `Around`.
                    "вместо" | "around" | "instead" => AnnotationKind::Instead,
                    "изменениеиконтроль" | "changeandvalidate" => {
                        AnnotationKind::ChangeAndValidate
                    }
                    _ => {
                        debug!(text = %token.text(), "unknown annotation kind, skipping");
                        return None;
                    }
                };

                Some(Annotation { kind, range })
            })
            .collect()
    }
}

fn is_inside_method(node: &SyntaxNode) -> bool {
    node.ancestors()
        .any(|n| n.kind() == SyntaxKind::PROCEDURE_DEF || n.kind() == SyntaxKind::FUNCTION_DEF)
}

/// End of the declaration header: after the export keyword when present, else
/// after the parameter list's closing `)`, else the name token. Anchors the full
/// signature slice and is bilingual by construction — it returns the offset of the
/// actual `Экспорт`/`Export` token, never a synthesised spelling.
fn header_end(
    export_keyword: Option<syntax::SyntaxToken>,
    param_list: Option<ast::ParamList>,
    name_range: text_size::TextRange,
) -> text_size::TextSize {
    use syntax::ast::AstNode;

    export_keyword
        .map(|t| t.text_range().end())
        .or_else(|| param_list.map(|pl| pl.syntax().text_range().end()))
        .unwrap_or_else(|| name_range.end())
}

fn calculate_params_content_range(param_list: &ast::ParamList) -> Option<text_size::TextRange> {
    use syntax::ast::AstNode;

    let params: Vec<_> = param_list.params().collect();
    if params.is_empty() {
        return None;
    }
    let first = params.first()?.syntax().text_range();
    let last = params.last()?.syntax().text_range();
    Some(text_size::TextRange::new(first.start(), last.end()))
}

pub fn lower_module_items_into(file: &ast::SourceFile, tree: &mut ItemTree) {
    let mut ctx = Ctx { tree: std::mem::take(tree), keys: MethodKeys::default() };
    ctx.lower_module_items(file);
    *tree = ctx.tree;
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_db::{Files, RootQueryDb, SourceDatabase};
    use vfs::{FileId, FileSet, VfsPath};

    #[salsa::db]
    #[derive(Default, Clone)]
    struct TestDb {
        storage: salsa::Storage<Self>,
        files: Files,
    }

    #[salsa::db]
    impl salsa::Database for TestDb {}

    #[salsa::db]
    impl SourceDatabase for TestDb {
        fn file_text_input(&self, file_id: FileId) -> base_db::FileTextInput {
            self.files.file_text(file_id)
        }

        fn try_file_text_input(&self, file_id: FileId) -> Option<base_db::FileTextInput> {
            self.files.try_file_text(file_id)
        }

        fn file_revision_input(&self, file_id: FileId) -> base_db::FileRevisionInput {
            self.files.file_revision(file_id)
        }

        fn try_file_revision_input(&self, file_id: FileId) -> Option<base_db::FileRevisionInput> {
            self.files.try_file_revision(file_id)
        }

        fn file_text(&self, file_id: FileId) -> std::sync::Arc<str> {
            self.file_text_ref(file_id).clone()
        }

        fn file_text_ref(&self, file_id: FileId) -> &std::sync::Arc<str> {
            let input = base_db::FileIdInput::new(self, file_id);
            base_db::file_text_query(self, input)
        }

        fn set_file_revision_from_disk(&mut self, file_id: FileId, revision: u64) {
            let files = self.files.clone();
            files.set_file_revision_from_disk(self, file_id, revision);
        }

        fn set_file_unreadable(&mut self, file_id: FileId) {
            let files = self.files.clone();
            files.set_file_unreadable(self, file_id);
        }

        fn file_is_unread(&self, file_id: FileId) -> bool {
            self.files.file_is_unread(self, file_id)
        }

        fn parse_snapshot(&self, file_id: FileId) -> Option<base_db::ParseSnapshot> {
            self.files.parse_snapshot(file_id)
        }

        fn store_parse_snapshot(&self, file_id: FileId, snapshot: base_db::ParseSnapshot) {
            self.files.store_parse_snapshot(file_id, snapshot);
        }

        fn count_parse(&self, outcome: base_db::ParseOutcome) {
            self.files.count_parse(outcome);
        }

        fn parse_stats(&self) -> base_db::ParseStats {
            self.files.parse_stats()
        }

        fn source_root_input(
            &self,
            source_root_id: base_db::SourceRootId,
        ) -> base_db::SourceRootInput {
            self.files.source_root(source_root_id)
        }

        fn try_source_root_input(
            &self,
            source_root_id: base_db::SourceRootId,
        ) -> Option<base_db::SourceRootInput> {
            self.files.try_source_root(source_root_id)
        }

        fn file_source_root_input(&self, file_id: FileId) -> base_db::FileSourceRootInput {
            self.files.file_source_root(file_id)
        }

        fn set_file_text(&mut self, file_id: FileId, text: &str) {
            let files = self.files.clone();
            files.set_file_text(self, file_id, text);
        }

        fn set_file_source_root(&mut self, file_id: FileId, source_root_id: base_db::SourceRootId) {
            let files = self.files.clone();
            files.set_file_source_root(self, file_id, source_root_id);
        }

        fn set_source_root(
            &mut self,
            source_root_id: base_db::SourceRootId,
            source_root: base_db::SourceRoot,
        ) {
            let files = self.files.clone();
            files.set_source_root(self, source_root_id, source_root);
        }

        fn resolve_vfs_path(
            &self,
            source_root_id: base_db::SourceRootId,
            vfs_path: &vfs::VfsPath,
        ) -> Option<FileId> {
            let source_root_input = self.source_root_input(source_root_id);
            let vfs_path_str = vfs_path.as_path().to_string_lossy().to_string();
            base_db::resolve_vfs_path_query(self, source_root_input, vfs_path_str)
        }
    }

    #[salsa::db]
    impl RootQueryDb for TestDb {
        fn parse(&self, file_id: FileId) -> syntax::Parse<syntax::SyntaxNode> {
            self.parse_ref(file_id).clone()
        }

        fn parse_ref(&self, file_id: FileId) -> &syntax::Parse<syntax::SyntaxNode> {
            let input = base_db::FileIdInput::new(self, file_id);
            base_db::parse_query(self, input)
        }

        fn method_regions(
            &self,
            file_id: FileId,
        ) -> std::sync::Arc<std::collections::HashMap<syntax::TextRange, String>> {
            let input = base_db::FileIdInput::new(self, file_id);
            base_db::method_regions_query(self, input)
        }
    }

    fn lower(input: &str) -> Arc<ItemTree> {
        let mut db = TestDb::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = base_db::SourceRoot::new_local(file_set);
        db.set_source_root(base_db::SourceRootId(0), source_root);
        db.set_file_source_root(file_id, base_db::SourceRootId(0));

        db.set_file_text(file_id, input);

        Ctx::lower_file(&db, file_id)
    }

    fn parse_var_def(input: &str) -> ast::VarDef {
        let mut db = TestDb::default();
        let file_id = FileId(0);

        let mut file_set = FileSet::new();
        file_set.insert(file_id, VfsPath::new("/test.bsl"));
        let source_root = base_db::SourceRoot::new_local(file_set);
        db.set_source_root(base_db::SourceRootId(0), source_root);
        db.set_file_source_root(file_id, base_db::SourceRootId(0));

        db.set_file_text(file_id, input);

        let parse = db.parse_ref(file_id);
        let file = ast::SourceFile::cast(parse.syntax_node()).expect("expected source file");

        file.syntax()
            .descendants()
            .find_map(ast::VarDef::cast)
            .expect("expected variable declaration")
    }

    #[test]
    fn test_empty_file() {
        let tree = lower("");
        assert_eq!(tree.top_level_items().len(), 0);
    }

    #[test]
    fn test_simple_procedure() {
        let tree = lower("Процедура Тест()\nКонецПроцедуры");

        assert_eq!(tree.top_level_items().len(), 1);
        assert!(matches!(tree.top_level_items()[0], ModItem::Procedure(_)));

        let proc = tree.procedure(match tree.top_level_items()[0] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });

        assert_eq!(proc.name.as_str(), "Тест");
        assert!(!proc.is_export);
        assert_eq!(proc.params.len(), 0);
    }

    #[test]
    fn test_export_function_with_params() {
        let input = "Функция СложитьЧисла(Знач Первое, Второе = 0) Экспорт\n    Возврат Первое + Второе;\nКонецФункции";
        let tree = lower(input);

        assert_eq!(tree.top_level_items().len(), 1);

        let func = tree.function(match tree.top_level_items()[0] {
            ModItem::Function(idx) => idx,
            _ => panic!("expected function"),
        });

        assert_eq!(func.name.as_str(), "СложитьЧисла");
        assert!(func.is_export);
        assert_eq!(func.params.len(), 2);

        assert_eq!(func.params[0].name.as_str(), "Первое");
        assert!(func.params[0].is_val);
        assert!(!func.params[0].has_default);

        assert_eq!(func.params[1].name.as_str(), "Второе");
        assert!(!func.params[1].is_val);
        assert!(func.params[1].has_default);
    }

    #[test]
    fn test_module_variable() {
        let input = "Перем ГлобальныйСчетчик Экспорт;";
        let tree = lower(input);

        assert_eq!(tree.top_level_items().len(), 1);

        let var = tree.variable(match tree.top_level_items()[0] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });

        assert_eq!(var.name.as_str(), "ГлобальныйСчетчик");
        assert!(var.is_export);
    }

    #[test]
    fn test_mixed_file() {
        let tree = lower(
            r#"
Перем Счетчик;

Процедура Инициализация() Экспорт
КонецПроцедуры

Функция ПолучитьСчетчик()
    Возврат Счетчик;
КонецФункции
        "#,
        );

        assert_eq!(tree.top_level_items().len(), 3);

        assert!(matches!(tree.top_level_items()[0], ModItem::Variable(_)));

        assert!(matches!(tree.top_level_items()[1], ModItem::Procedure(_)));

        assert!(matches!(tree.top_level_items()[2], ModItem::Function(_)));
    }

    #[test]
    fn test_procedure_with_annotation_client() {
        let tree = lower(
            r#"
&НаКлиенте
Процедура КлиентскаяПроцедура()
КонецПроцедуры
            "#,
        );

        let proc = tree.procedure(match tree.top_level_items()[0] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });

        assert_eq!(proc.name.as_str(), "КлиентскаяПроцедура");
        assert_eq!(proc.annotations.len(), 1);
        assert!(matches!(proc.annotations[0].kind, AnnotationKind::AtClient));
    }

    #[test]
    fn test_function_with_annotation_server() {
        let tree = lower(
            r#"
&НаСервере
Функция СерверныйМетод()
КонецФункции
            "#,
        );

        let func = tree.function(match tree.top_level_items()[0] {
            ModItem::Function(idx) => idx,
            _ => panic!("expected function"),
        });

        assert_eq!(func.name.as_str(), "СерверныйМетод");
        assert_eq!(func.annotations.len(), 1);
        assert!(matches!(func.annotations[0].kind, AnnotationKind::AtServer));
    }

    #[test]
    fn test_procedure_with_multiple_params() {
        let tree = lower(
            r#"
Процедура СложнаяПроцедура(Знач Первый, Второй, Знач Третий = 10, Четвертый = "текст")
КонецПроцедуры
            "#,
        );

        let proc = tree.procedure(match tree.top_level_items()[0] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });

        assert_eq!(proc.params.len(), 4);

        assert_eq!(proc.params[0].name.as_str(), "Первый");
        assert!(proc.params[0].is_val);
        assert!(!proc.params[0].has_default);

        assert_eq!(proc.params[1].name.as_str(), "Второй");
        assert!(!proc.params[1].is_val);
        assert!(!proc.params[1].has_default);

        assert_eq!(proc.params[2].name.as_str(), "Третий");
        assert!(proc.params[2].is_val);
        assert!(proc.params[2].has_default);

        assert_eq!(proc.params[3].name.as_str(), "Четвертый");
        assert!(!proc.params[3].is_val);
        assert!(proc.params[3].has_default);
    }

    #[test]
    fn test_english_keywords() {
        let tree = lower(
            r#"
Var GlobalCounter Export;

Procedure Initialize() Export
EndProcedure

Function GetCounter()
    Return GlobalCounter;
EndFunction
            "#,
        );

        assert_eq!(tree.top_level_items().len(), 3);

        let var = tree.variable(match tree.top_level_items()[0] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });
        assert_eq!(var.name.as_str(), "GlobalCounter");
        assert!(var.is_export);

        let proc = tree.procedure(match tree.top_level_items()[1] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });
        assert_eq!(proc.name.as_str(), "Initialize");
        assert!(proc.is_export);

        let func = tree.function(match tree.top_level_items()[2] {
            ModItem::Function(idx) => idx,
            _ => panic!("expected function"),
        });
        assert_eq!(func.name.as_str(), "GetCounter");
        assert!(!func.is_export);
    }

    #[test]
    fn test_annotation_client_at_server() {
        let tree = lower(
            r#"
&НаКлиентеНаСервере
Функция УниверсальнаяФункция()
КонецФункции
            "#,
        );

        let func = tree.function(match tree.top_level_items()[0] {
            ModItem::Function(idx) => idx,
            _ => panic!("expected function"),
        });

        assert_eq!(func.annotations.len(), 1);
        assert!(matches!(func.annotations[0].kind, AnnotationKind::AtClientAtServer));
    }

    #[test]
    fn test_annotation_english() {
        let tree = lower(
            r#"
&AtClient
Procedure ClientProcedure()
EndProcedure

&AtServer
Function ServerFunction()
EndFunction
            "#,
        );

        assert_eq!(tree.top_level_items().len(), 2);

        let proc = tree.procedure(match tree.top_level_items()[0] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });
        assert_eq!(proc.annotations.len(), 1);
        assert!(matches!(proc.annotations[0].kind, AnnotationKind::AtClient));

        let func = tree.function(match tree.top_level_items()[1] {
            ModItem::Function(idx) => idx,
            _ => panic!("expected function"),
        });
        assert_eq!(func.annotations.len(), 1);
        assert!(matches!(func.annotations[0].kind, AnnotationKind::AtServer));
    }

    #[test]
    fn test_multiple_variables() {
        let tree = lower(
            r#"
Перем Первая;
Перем Вторая Экспорт;
Перем Третья;
            "#,
        );

        assert_eq!(tree.top_level_items().len(), 3);

        let var1 = tree.variable(match tree.top_level_items()[0] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });
        assert_eq!(var1.name.as_str(), "Первая");
        assert!(!var1.is_export);

        let var2 = tree.variable(match tree.top_level_items()[1] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });
        assert_eq!(var2.name.as_str(), "Вторая");
        assert!(var2.is_export);

        let var3 = tree.variable(match tree.top_level_items()[2] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });
        assert_eq!(var3.name.as_str(), "Третья");
        assert!(!var3.is_export);
    }

    #[test]
    fn module_comma_var_decl_lowers_each_name() {
        let input = "Перем Первая, Вторая Экспорт;";
        let tree = lower(input);
        let var_def = parse_var_def(input);
        let expected_name_ranges: Vec<_> = var_def.names().map(|name| name.text_range()).collect();
        let expected_source_range = var_def.syntax().text_range();

        assert_eq!(tree.top_level_items().len(), 2);

        let var1 = tree.variable(match tree.top_level_items()[0] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });
        let var2 = tree.variable(match tree.top_level_items()[1] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });

        assert_eq!(var1.name.as_str(), "Первая");
        assert_eq!(var2.name.as_str(), "Вторая");
        // `Экспорт` stands after `Вторая` only: the platform exports it alone.
        assert!(!var1.is_export);
        assert!(var2.is_export);
        assert!(var1.annotations.is_empty());
        assert_eq!(var1.annotations, var2.annotations);
        assert_eq!(var1.source_range, expected_source_range);
        assert_eq!(var2.source_range, expected_source_range);
        assert_eq!(var1.name_range, expected_name_ranges[0]);
        assert_eq!(var2.name_range, expected_name_ranges[1]);
        assert_ne!(var1.name_range, var2.name_range);
    }

    #[test]
    fn export_after_a_name_exports_that_name_alone() {
        for (input, expected) in [
            ("Перем Первая Экспорт, Вторая;", [("Первая", true), ("Вторая", false)]),
            ("Перем Первая Экспорт, Вторая Экспорт;", [("Первая", true), ("Вторая", true)]),
            ("Перем Первая, Вторая;", [("Первая", false), ("Вторая", false)]),
        ] {
            let tree = lower(input);
            let lowered: Vec<(String, bool)> = tree
                .top_level_items()
                .iter()
                .map(|item| match item {
                    ModItem::Variable(idx) => {
                        let var = tree.variable(*idx);
                        (var.name.as_str().to_string(), var.is_export)
                    }
                    _ => panic!("expected variable"),
                })
                .collect();
            let expected: Vec<(String, bool)> =
                expected.iter().map(|(name, export)| (name.to_string(), *export)).collect();
            assert_eq!(lowered, expected, "{input}");
        }
    }
    #[test]
    fn module_comma_var_decl_under_pre_if_lowers_each_name() {
        let input = r#"
#Если Сервер Тогда
    Перем Внутри, ЕщеВнутри;
#КонецЕсли
        "#;
        let tree = lower(input);
        let var_def = parse_var_def(input);
        let expected_name_ranges: Vec<_> = var_def.names().map(|name| name.text_range()).collect();
        let expected_source_range = var_def.syntax().text_range();

        assert!(tree.has_module_preproc());
        assert_eq!(tree.top_level_items().len(), 2);

        let var1 = tree.variable(match tree.top_level_items()[0] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });
        let var2 = tree.variable(match tree.top_level_items()[1] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });

        assert_eq!(var1.name.as_str(), "Внутри");
        assert_eq!(var2.name.as_str(), "ЕщеВнутри");
        assert!(!var1.is_export);
        assert!(!var2.is_export);
        assert!(var1.annotations.is_empty());
        assert_eq!(var1.annotations, var2.annotations);
        assert_eq!(var1.source_range, expected_source_range);
        assert_eq!(var2.source_range, expected_source_range);
        assert_eq!(var1.name_range, expected_name_ranges[0]);
        assert_eq!(var2.name_range, expected_name_ranges[1]);
        assert_ne!(var1.name_range, var2.name_range);
    }

    #[test]
    fn module_comma_var_decl_english_export_lowers_each_name() {
        let input = r#"
&AtServer
Var Alpha, Beta Export;
        "#;
        let tree = lower(input);
        let var_def = parse_var_def(input);
        let expected_name_ranges: Vec<_> = var_def.names().map(|name| name.text_range()).collect();
        let expected_source_range = var_def.syntax().text_range();
        let expected_annotations = var_def
            .annotations()
            .map(|annotation| Annotation {
                kind: AnnotationKind::AtServer,
                range: annotation.syntax().text_range(),
            })
            .collect::<Vec<_>>();

        assert_eq!(tree.top_level_items().len(), 2);

        let var1 = tree.variable(match tree.top_level_items()[0] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });
        let var2 = tree.variable(match tree.top_level_items()[1] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });

        assert_eq!(var1.name.as_str(), "Alpha");
        assert_eq!(var2.name.as_str(), "Beta");
        assert!(!var1.is_export);
        assert!(var2.is_export);
        assert_eq!(var1.annotations.as_ref(), expected_annotations.as_slice());
        assert_eq!(var2.annotations.as_ref(), expected_annotations.as_slice());
        assert_eq!(var1.source_range, expected_source_range);
        assert_eq!(var2.source_range, expected_source_range);
        assert_eq!(var1.name_range, expected_name_ranges[0]);
        assert_eq!(var2.name_range, expected_name_ranges[1]);
        assert_ne!(var1.name_range, var2.name_range);
    }

    #[test]
    fn test_malformed_variable_decl_emits_nothing() {
        let tree = lower("Перем;");

        assert_eq!(tree.top_level_items().len(), 0);
        assert_eq!(tree.variables().count(), 0);
    }

    #[test]
    fn test_variable_with_annotation() {
        let tree = lower(
            r#"
&НаСервере
Перем СерверныйСчетчик;
            "#,
        );

        let var = tree.variable(match tree.top_level_items()[0] {
            ModItem::Variable(idx) => idx,
            _ => panic!("expected variable"),
        });

        assert_eq!(var.name.as_str(), "СерверныйСчетчик");
        assert_eq!(var.annotations.len(), 1);
        assert!(matches!(var.annotations[0].kind, AnnotationKind::AtServer));
    }

    #[test]
    fn test_extension_annotation_with_params() {
        let tree = lower(
            r#"&Вместо("ОригинальныйМетод")
Процедура РасширениеМетода()
КонецПроцедуры"#,
        );

        assert_eq!(tree.top_level_items().len(), 1);

        let proc = tree.procedure(match tree.top_level_items()[0] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });

        assert_eq!(proc.name.as_str(), "РасширениеМетода");
        assert_eq!(proc.annotations.len(), 1);
        assert!(matches!(proc.annotations[0].kind, AnnotationKind::Instead));
    }

    /// Английское имя `Вместо` — `Around` (4.8.2 руководства разработчика).
    #[test]
    fn around_is_the_english_name_of_instead() {
        let tree = lower("&Around(\"М\")\nПроцедура Р()\nКонецПроцедуры");
        let proc = tree.procedure(match tree.top_level_items()[0] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });
        assert_eq!(proc.annotations.len(), 1, "аннотация &Around потеряна при lowering");
        assert!(matches!(proc.annotations[0].kind, AnnotationKind::Instead));
    }

    /// Слой написаний согласован с лексером, а не только с обычным регистром.
    ///
    /// `(?i)` в logos складывает регистр по Unicode, и `ſ` (U+017F) там равна
    /// `s`. Простое приведение к нижнему регистру этого не делает, поэтому
    /// написание, которое лексер принял, теряло бы семантику.
    #[test]
    fn annotation_spelling_folds_case_the_way_the_lexer_does() {
        let tree = lower("&At\u{017F}erver\nПроцедура Р()\nКонецПроцедуры");
        let proc = tree.procedure(match tree.top_level_items()[0] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });
        assert_eq!(
            proc.annotations.len(),
            1,
            "написание, принятое лексером, потеряно при lowering"
        );
        assert!(matches!(proc.annotations[0].kind, AnnotationKind::AtServer));
    }

    /// Регистр во встроенном языке не значим — 4.2.4.5.
    #[test]
    fn annotation_spelling_is_case_insensitive() {
        let tree = lower("&вместо(\"М\")\nПроцедура Р()\nКонецПроцедуры");
        let proc = tree.procedure(match tree.top_level_items()[0] {
            ModItem::Procedure(idx) => idx,
            _ => panic!("expected procedure"),
        });
        assert_eq!(proc.annotations.len(), 1, "строчное написание потеряно при lowering");
    }
}
