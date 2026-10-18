use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::execution_env::EnvFlags;
use hir::LocalRange;
use hir::{EnvMemberKind, Name};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious, MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

pub fn from_hir(
    name: &Name,
    member_kind: EnvMemberKind,
    missing: EnvFlags,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let (kind_ru, suffix_ru) = match member_kind {
        EnvMemberKind::Method => ("Метод", "недоступен"),
        EnvMemberKind::Property => ("Свойство", "недоступно"),
        EnvMemberKind::GlobalFunction => ("Глобальная функция", "недоступна"),
        EnvMemberKind::GlobalProperty => ("Глобальное свойство", "недоступно"),
        EnvMemberKind::GlobalVariable => ("Глобальная переменная", "недоступна"),
        EnvMemberKind::Type => ("Тип", "недоступен"),
    };
    let envs: Vec<&str> = missing.iter().map(|flag| flag.name_ru()).collect();
    let message = format!("{} '{}' {} [{}]", kind_ru, name.as_str(), suffix_ru, envs.join(", "));
    crate::simple_hir_diagnostic(DiagnosticCode::UnavailableInEnvironment, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_hir_diagnostic_with_fixtures;
    use crate::DiagnosticCode;

    fn env_diags(fixture: &str) -> Vec<(String, String)> {
        check_hir_diagnostic_with_fixtures(fixture)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnavailableInEnvironment)
            .map(|d| (d.message, String::new()))
            .collect()
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn server_only_type_method_flagged_in_client_form_method() {
        // ЧтениеТекста is unavailable in the web client; a form method behind
        // &НаКлиенте runs in every configured client environment.
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Функция ПрочитатьНаСервере()
    Чтение = Новый ЧтениеТекста;
    Возврат Чтение.ПрочитатьСтроку();
КонецФункции
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "server context admits ЧтениеТекста, got: {diags:?}");

        let fixture_client = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Функция ПрочитатьНаКлиенте()
    Чтение = Новый ЧтениеТекста;
    Возврат Чтение.ПрочитатьСтроку();
КонецФункции
"#;
        let diags = env_diags(fixture_client);
        assert_eq!(
            diags.len(),
            2,
            "web client lacks both the type and its methods, got: {diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.0.starts_with("Тип 'ЧтениеТекста' недоступен")),
            "constructor must be flagged: {diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.0.starts_with("Метод 'ПрочитатьСтроку' недоступен")),
            "method call must be flagged: {diags:?}"
        );
        for (message, _) in &diags {
            assert!(
                message.contains("Веб-клиент") && !message.contains("Тонкий клиент"),
                "only the web client lacks ЧтениеТекста: {message}"
            );
        }
    }

    /// Имя типа, записанное строкой, ограничено контекстом ровно так же, как
    /// записанное синтаксисом.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn server_only_type_flagged_when_named_by_string() {
        for ctor in [r#"Новый("ЧтениеТекста")"#, r#"Новый(Тип("ЧтениеТекста"))"#]
        {
            let fixture = format!(
                r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура ПрочитатьНаКлиенте()
    Чтение = {ctor};
КонецПроцедуры
"#
            );
            let diags = env_diags(&fixture);
            assert!(
                diags.iter().any(|d| d.0.starts_with("Тип 'ЧтениеТекста' недоступен")),
                "`{ctor}` names the same server-only type and must be flagged: {diags:?}"
            );
        }
    }

    /// Квалифицированное имя — объект конфигурации, а не платформенный тип:
    /// доступности по контексту у него нет и повода жаловаться тоже.
    #[test]
    fn qualified_string_type_name_is_not_env_checked() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура НаКлиенте()
    Ключ = Новый(Тип("РегистрСведенийКлючЗаписи.ЦеныНоменклатуры"));
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "a configuration object carries no environment mask: {diags:?}");
    }

    #[test]
    fn preprocessor_guard_narrows_environments() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    #Если НЕ ВебКлиент Тогда
    Чтение = Новый ЧтениеТекста;
    Стр = Чтение.ПрочитатьСтроку();
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "the guard excludes the web client, got: {diags:?}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn preprocessor_branch_still_checks_matching_environments() {
        // Unlike a blanket skip, narrowing keeps checking the environments
        // the branch IS compiled for.
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    #Если ТонкийКлиент ИЛИ ВебКлиент Тогда
    Чтение = Новый ЧтениеТекста;
    Стр = Чтение.ПрочитатьСтроку();
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert_eq!(diags.len(), 2, "web client is inside the branch mask, got: {diags:?}");
        for (message, _) in &diags {
            assert!(
                message.contains("Веб-клиент") && !message.contains("Тонкий клиент"),
                "only the web client lacks ЧтениеТекста: {message}"
            );
        }
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn preprocessor_else_gets_complement() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    #Если ВебКлиент Тогда
    Сообщить("веб");
    #Иначе
    Чтение = Новый ЧтениеТекста;
    Стр = Чтение.ПрочитатьСтроку();
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "else runs only on thin/thick clients, got: {diags:?}");

        let fixture_positive = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиентеНаСервере
Процедура Записать()
    #Если Сервер Тогда
    Сообщить("сервер");
    #Иначе
    ЗаписьЖурналаРегистрации("Событие");
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture_positive);
        assert_eq!(diags.len(), 1, "else covers the clients, got: {diags:?}");
        assert!(
            diags[0].0.contains("Тонкий клиент") && diags[0].0.contains("Веб-клиент"),
            "thin and web clients lack the API: {}",
            diags[0].0
        );
    }

    #[test]
    fn unrecognized_preprocessor_symbol_skips_chain() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    #Если Линукс Тогда
    Чтение = Новый ЧтениеТекста;
    Стр = Чтение.ПрочитатьСтроку();
    #Иначе
    ЗаписьЖурналаРегистрации("Событие");
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(
            diags.is_empty(),
            "an undecidable condition must silence the whole chain, got: {diags:?}"
        );
    }

    #[test]
    fn malformed_preprocessor_condition_silences_chain() {
        // Parser error recovery may keep a valid prefix of the condition;
        // the availability check must not act on it.
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    #Если ВебКлиент = 1 Тогда
    Чтение = Новый ЧтениеТекста;
    Стр = Чтение.ПрочитатьСтроку();
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "malformed condition must silence the chain, got: {diags:?}");
    }

    #[test]
    fn nested_preprocessor_narrows_cumulatively() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    #Если Клиент Тогда
        #Если НЕ ВебКлиент Тогда
        Чтение = Новый ЧтениеТекста;
        Стр = Чтение.ПрочитатьСтроку();
        #КонецЕсли
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "nested guards intersect, got: {diags:?}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn server_only_global_function_flagged_on_client() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    ЗаписьЖурналаРегистрации("Событие");
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert_eq!(
            diags.len(),
            1,
            "ЗаписьЖурналаРегистрации is not available on thin/web clients, got: {diags:?}"
        );
        assert!(
            diags[0].0.contains("Тонкий клиент") && diags[0].0.contains("Веб-клиент"),
            "qualifier must list both missing client environments: {}",
            diags[0].0
        );
        assert!(
            !diags[0].0.contains("управляемое приложение"),
            "thick client is available — must not be reported: {}",
            diags[0].0
        );
    }

    #[test]
    fn server_only_system_enum_root_is_flagged_on_client() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Прочитать()
    Значение = АлгоритмПодписиТокенаДоступа;
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert_eq!(diags.len(), 1, "system-enum root carries catalog availability: {diags:?}");
        assert!(diags[0].0.contains("Тонкий клиент") && diags[0].0.contains("Веб-клиент"));
    }

    #[test]
    fn module_level_preprocessor_guard_narrows_method_env() {
        // The guard wraps the whole procedure definition, not statements
        // inside its body — the method never compiles for the web client.
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
#Если НЕ ВебКлиент Тогда
&НаКлиенте
Процедура Прочитать()
    Чтение = Новый ЧтениеТекста;
    Стр = Чтение.ПрочитатьСтроку();
КонецПроцедуры
#КонецЕсли
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "module-level guard excludes the web client, got: {diags:?}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn module_level_guard_still_checks_matching_environments() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
#Если ТонкийКлиент ИЛИ ВебКлиент Тогда
&НаКлиенте
Процедура Прочитать()
    Чтение = Новый ЧтениеТекста;
КонецПроцедуры
#КонецЕсли
"#;
        let diags = env_diags(fixture);
        assert_eq!(diags.len(), 1, "web client stays inside the module-level mask, got: {diags:?}");
        assert!(
            diags[0].0.contains("Веб-клиент") && !diags[0].0.contains("Тонкий клиент"),
            "only the web client lacks ЧтениеТекста: {}",
            diags[0].0
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn type_constructor_unavailable_in_web_client() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Сгенерировать()
    Генератор = Новый ГенераторСлучайныхЧисел(42);
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert_eq!(
            diags.len(),
            1,
            "ГенераторСлучайныхЧисел cannot be constructed in the web client, got: {diags:?}"
        );
        assert!(
            diags[0].0.starts_with("Тип 'ГенераторСлучайныхЧисел' недоступен")
                && diags[0].0.contains("Веб-клиент"),
            "message must name the type and the missing environment: {}",
            diags[0].0
        );
    }

    #[test]
    fn universal_type_constructor_not_flagged() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Собрать()
    Список = Новый Массив;
    Соответствие = Новый Соответствие;
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "universally available types must stay silent, got: {diags:?}");
    }

    /// Глобальные коллекции менеджеров (`Перечисления`, `Справочники`, …) —
    /// серверная поверхность: тонкий и веб-клиент их не компилируют.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn manager_collection_flagged_on_client() {
        let fixture_server = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Процедура УстановитьНаСервере()
    ЭтоДействие = Перечисления.ВидыТочекМаршрута.Действие;
КонецПроцедуры
"#;
        let diags = env_diags(fixture_server);
        assert!(diags.is_empty(), "server context admits Перечисления, got: {diags:?}");

        let fixture_client = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура УстановитьДоступность()
    ЭтоДействие = Перечисления.ВидыТочекМаршрута.Действие;
КонецПроцедуры
"#;
        let diags = env_diags(fixture_client);
        assert_eq!(
            diags.len(),
            1,
            "the collection root must be flagged exactly once, got: {diags:?}"
        );
        assert!(
            diags[0].0.starts_with("Глобальное свойство 'Перечисления' недоступно"),
            "message must name the global property: {}",
            diags[0].0
        );
        assert!(
            diags[0].0.contains("Тонкий клиент") && diags[0].0.contains("Веб-клиент"),
            "thin and web clients lack manager collections: {}",
            diags[0].0
        );
        assert!(
            !diags[0].0.contains("управляемое приложение"),
            "thick client admits manager collections — must not be reported: {}",
            diags[0].0
        );
    }

    /// Присваивание коллекции локаль НЕ объявляет: имя принадлежит свойству
    /// глобального контекста, и платформа отвергает запись, а не создаёт
    /// переменную. Поэтому обращение остаётся обращением к коллекции и
    /// ограничение по средам к нему применяется — до присваивания и после.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn assignment_does_not_silence_collection_env() {
        for (label, fixture) in [
            (
                "assignment before the read",
                r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Установить(Данные)
    Перечисления = Данные.Коллекция;
    ЭтоДействие = Перечисления.ВидыТочекМаршрута.Действие;
КонецПроцедуры
"#,
            ),
            (
                "assignment after the read",
                r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Установить(Данные)
    ЭтоДействие = Перечисления.ВидыТочекМаршрута.Действие;
    Перечисления = Данные.Коллекция;
КонецПроцедуры
"#,
            ),
        ] {
            let diags = env_diags(fixture);
            assert_eq!(diags.len(), 1, "{label}: the read is still the collection, got: {diags:?}");
        }
    }

    /// Модульная переменная забирает имя коллекции и в вызывной форме цепочки,
    /// а не только там, где корень читают отдельно.
    #[test]
    fn module_var_shadows_three_level_root() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
Перем Справочники;

&НаКлиенте
Процедура Тест()
    Х = Справочники.Товары.НайтиПоКоду("1");
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "a module variable shadows the global, got: {diags:?}");
    }

    /// Общий модуль конфигурации с именем коллекции затеняет платформенный
    /// глобал — доступность глобала не применяется.
    #[test]
    fn workspace_module_shadows_manager_collection() {
        let fixture = r#"
//- /CommonModules/Перечисления/Ext/Module.bsl
Функция Получить() Экспорт
    Возврат 1;
КонецФункции
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура Тест()
    Х = Перечисления;
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "a workspace common module shadows the global, got: {diags:?}");
    }

    /// Диагностика вызывной формы стоит на корневом имени коллекции,
    /// а не на всём вызове.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn three_level_diagnostic_anchors_on_collection_root() {
        let source = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура НайтиНаКлиенте()
    Товар = Справочники.Товары.НайтиПоКоду("1");
КонецПроцедуры
"#;
        let content = source.split_once("Module.bsl\n").expect("fixture header present").1;
        let diag = check_hir_diagnostic_with_fixtures(source)
            .into_iter()
            .find(|d| d.code == DiagnosticCode::UnavailableInEnvironment)
            .expect("the call root must be flagged");
        let flagged = &content[usize::from(diag.range.start())..usize::from(diag.range.end())];
        assert_eq!(
            flagged, "Справочники",
            "the diagnostic must anchor on the collection root name"
        );
    }

    /// Реквизит формы с именем коллекции затеняет платформенный глобал:
    /// в клиентском коде имя обозначает реквизит, а не серверную коллекцию.
    #[test]
    fn form_attribute_shadows_manager_collection() {
        let form_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <Properties>
        <Name>ФормаЭлемента</Name>
    </Properties>
    <Attributes>
        <Attribute name="Перечисления" id="1">
            <Type/>
        </Attribute>
    </Attributes>
</Form>"#;
        let source = r#"&НаКлиенте
Процедура Тест()
    Х = Перечисления.ВидыТочекМаршрута.Действие;
КонецПроцедуры
"#;
        let diags: Vec<String> = crate::test_utils::check_form_with_form_xml(source, form_xml)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnavailableInEnvironment)
            .map(|d| d.message)
            .collect();
        assert!(diags.is_empty(), "a form attribute shadows the global, got: {diags:?}");
    }

    /// Присваивание в серверной ветке `#Иначе` больше не глушит проверку в
    /// клиентской ветке — не потому, что затенение стало учитывать ветки, а
    /// потому что присваивание коллекции не объявляет локаль вовсе, так что
    /// вопрос о ветках не возникает. Прежний осознанный FN закрылся сам.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn cross_branch_assignment_does_not_silence_the_client_branch() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиентеНаСервере
Процедура Тест()
    #Если Клиент Тогда
    Х = Перечисления.ВидыТочекМаршрута.Действие;
    #Иначе
    Перечисления = Новый Структура;
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert_eq!(
            diags.len(),
            1,
            "an assignment declares no local, so the client-branch read stays checked, got: {diags:?}"
        );
    }

    /// Корень, обёрнутый в скобки, — по-прежнему трёхуровневый вызов;
    /// диагностика стоит на имени, а не на всём выражении.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn parenthesized_three_level_root_anchors_on_name() {
        let source = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура НайтиНаКлиенте()
    Товар = (Справочники).Товары.НайтиПоКоду("1");
КонецПроцедуры
"#;
        let content = source.split_once("Module.bsl\n").expect("fixture header present").1;
        let diag = check_hir_diagnostic_with_fixtures(source)
            .into_iter()
            .find(|d| d.code == DiagnosticCode::UnavailableInEnvironment)
            .expect("the call root must be flagged");
        let flagged = &content[usize::from(diag.range.start())..usize::from(diag.range.end())];
        assert_eq!(
            flagged, "Справочники",
            "the diagnostic must anchor on the collection root name"
        );
    }

    /// Клиентская замена — `ПредопределенноеЗначение` со строковым именем:
    /// строка не резолвится в глобальное свойство и жалоб не даёт.
    #[test]
    fn predefined_value_replacement_not_flagged() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура УстановитьДоступность()
    ЭтоДействие = ПредопределенноеЗначение("Перечисление.ВидыТочекМаршрута.Действие");
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "the client-safe replacement must stay silent, got: {diags:?}");
    }

    /// Английское имя коллекции ограничено так же, как русское.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn english_manager_collection_flagged_on_client() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура УстановитьДоступность()
    ЭтоДействие = Enums.ВидыТочекМаршрута.Действие;
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert_eq!(diags.len(), 1, "English spelling names the same global, got: {diags:?}");
        assert!(
            diags[0].0.starts_with("Глобальное свойство 'Enums' недоступно"),
            "message must carry the spelling as written: {}",
            diags[0].0
        );
    }

    /// Корень цепочки проверяется и в вызывной форме
    /// `Справочники.Товары.НайтиПоКоду()`, а не только в форме чтения.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn three_level_manager_call_flagged_on_client() {
        let fixture_server = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаСервере
Процедура НайтиНаСервере()
    Товар = Справочники.Товары.НайтиПоКоду("1");
КонецПроцедуры
"#;
        let diags = env_diags(fixture_server);
        assert!(diags.is_empty(), "server context admits Справочники, got: {diags:?}");

        let fixture_client = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиенте
Процедура НайтиНаКлиенте()
    Товар = Справочники.Товары.НайтиПоКоду("1");
КонецПроцедуры
"#;
        let diags = env_diags(fixture_client);
        assert_eq!(diags.len(), 1, "the call root must be flagged exactly once, got: {diags:?}");
        assert!(
            diags[0].0.starts_with("Глобальное свойство 'Справочники' недоступно"),
            "message must name the collection root: {}",
            diags[0].0
        );
    }

    /// Сужение `#Если Сервер` действует и на вызывную форму цепочки: под
    /// серверным guard'ом обращение к коллекции с клиента не нарушение.
    #[test]
    fn three_level_call_under_server_guard_not_flagged() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиентеНаСервере
Процедура Найти()
    #Если Сервер Тогда
    Товар = Справочники.Товары.НайтиПоКоду("1");
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "the guard leaves only server environments, got: {diags:?}");
    }

    /// `#Если Сервер` сужает среды тела — внутри ветки коллекции доступны.
    #[test]
    fn manager_collection_under_server_guard_not_flagged() {
        let fixture = r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
&НаКлиентеНаСервере
Процедура Установить()
    #Если Сервер Тогда
    ЭтоДействие = Перечисления.ВидыТочекМаршрута.Действие;
    #КонецЕсли
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "the guard leaves only server environments, got: {diags:?}");
    }

    #[test]
    fn common_module_without_client_flag_not_checked_against_client() {
        // A server common module calling server API — no diagnostics.
        let fixture = r#"
//- /CommonModules/Серверный/Ext/Module.bsl
Процедура Прочитать() Экспорт
    Чтение = Новый ЧтениеТекста;
    Стр = Чтение.ПрочитатьСтроку();
КонецПроцедуры
"#;
        let diags = env_diags(fixture);
        assert!(diags.is_empty(), "server module may use server API, got: {diags:?}");
    }

    const FORM_COLLECTIONS_OWNER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <DataProcessor uuid="00000000-0000-0000-0000-000000000101">
        <Properties><Name>Товары</Name></Properties>
        <ChildObjects>
            <Attribute uuid="00000000-0000-0000-0000-000000000104">
                <Properties><Name>ТаблицаОбъекта</Name><Type><v8:Type>v8:ValueTable</v8:Type></Type></Properties>
            </Attribute>
            <TabularSection uuid="00000000-0000-0000-0000-000000000102">
                <Properties><Name>Состав</Name></Properties>
                <ChildObjects>
                    <Attribute uuid="00000000-0000-0000-0000-000000000103">
                        <Properties><Name>Код</Name><Type><v8:Type>xs:decimal</v8:Type></Type></Properties>
                    </Attribute>
                </ChildObjects>
            </TabularSection>
        </ChildObjects>
    </DataProcessor>
</MetaDataObject>"#;

    const FORM_COLLECTIONS_FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <ChildItems>
        <Table name="Состав" id="1">
            <DataPath>Объект.Состав</DataPath>
        </Table>
    </ChildItems>
    <Attributes>
        <Attribute name="Объект" id="1">
            <Type><v8:Type>cfg:DataProcessorObject.Товары</v8:Type></Type>
            <MainAttribute>true</MainAttribute>
        </Attribute>
        <Attribute name="Таблица" id="2">
            <Type><v8:Type>v8:ValueTable</v8:Type></Type>
        </Attribute>
        <Attribute name="Второй" id="3">
            <Type><v8:Type>cfg:DataProcessorObject.Товары</v8:Type></Type>
        </Attribute>
    </Attributes>
</Form>"#;

    fn form_collection_diags(source: &str) -> Vec<(String, String)> {
        crate::test_utils::check_form_with_owner_xml(
            source,
            FORM_COLLECTIONS_FORM_XML,
            FORM_COLLECTIONS_OWNER_XML,
        )
        .into_iter()
        .filter(|d| {
            matches!(
                d.code,
                DiagnosticCode::UnavailableInEnvironment | DiagnosticCode::UnresolvedMethodCall
            )
        })
        .map(|d| (format!("{:?}", d.code), d.message))
        .collect()
    }

    /// На клиенте табличная часть основного и любого другого реквизита-объекта, её
    /// строка, реквизит объекта и реквизит формы типа `ТаблицаЗначений` - это
    /// `ДанныеФормыКоллекция` и `ДанныеФормыЭлементКоллекции`: `Добавить`,
    /// `НайтиСтроки`, `Количество`, `Индекс`, `Удалить`, `Очистить` и `НомерСтроки`
    /// строки выполняются в тонком и веб-клиенте. Контроли: `ВыгрузитьКолонку` у
    /// коллекции на клиенте нет вовсе, а `Выгрузить` выполняется только на сервере -
    /// обе находки остаются.
    #[test]
    fn form_data_collections_available_in_thin_and_web_client() {
        let source = r#"&НаКлиенте
Процедура Тест()
    Строка = Объект.Состав.Добавить();
    Строка.Код = 1;
    Количество = Объект.Состав.Количество();
    Найденные = Объект.Состав.НайтиСтроки(Новый Структура("Код", 1));
    Для Каждого Найденная Из Найденные Цикл
        Номер = Найденная.НомерСтроки;
    КонецЦикла;
    Индекс = Объект.Состав.Индекс(Строка);
    Объект.Состав.Удалить(Строка);
    Объект.Состав.Очистить();
    Текущие = Элементы.Состав.ТекущиеДанные;
    Номер = Текущие.НомерСтроки;
    Найденные = Таблица.НайтиСтроки(Новый Структура("Код", 1));
    Количество = Таблица.Количество();
    Таблица.Очистить();
    СтрокаОбъекта = Объект.ТаблицаОбъекта.Добавить();
    Найденные = Объект.ТаблицаОбъекта.НайтиСтроки(Новый Структура("Код", 1));
    Количество = Объект.ТаблицаОбъекта.Количество();
    СтрокаВторого = Второй.Состав.Добавить();
    Количество = Второй.Состав.Количество();
    Для Каждого СтрокаВторого Из Второй.Состав Цикл
        Номер = СтрокаВторого.НомерСтроки;
    КонецЦикла;
    Колонка = Объект.Состав.ВыгрузитьКолонку("Код");
    Выгрузка = Объект.Состав.Выгрузить();
КонецПроцедуры
"#;
        let diags = form_collection_diags(source);
        let messages: Vec<&str> = diags.iter().map(|(_, message)| message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "Метод 'ВыгрузитьКолонку' не найден у 'Обработки.Товары.Состав'",
                "Метод 'Выгрузить' недоступен \
                 [Тонкий клиент, Веб-клиент, Толстый клиент (управляемое приложение)]",
            ],
            "only the two controls may be reported, got: {diags:?}"
        );
    }

    /// Реквизит формы и реквизит объекта типа `ТаблицаЗначений` - тоже
    /// `ДанныеФормыКоллекция`, и на сервере тоже: члена `ТаблицаЗначений` у них нет ни
    /// на клиенте, ни на сервере. Значением таблицы реквизит становится только через
    /// `РеквизитФормыВЗначение`.
    #[test]
    fn value_table_form_attribute_is_form_data_collection_on_both_sides() {
        let client = r#"&НаКлиенте
Процедура Тест()
    Колонка = Таблица.ВыгрузитьКолонку("Код");
    Колонка = Объект.ТаблицаОбъекта.ВыгрузитьКолонку("Код");
КонецПроцедуры
"#;
        let diags = form_collection_diags(client);
        let expected = (
            "UnresolvedMethodCall".to_string(),
            "Метод 'ВыгрузитьКолонку' не найден у 'ДанныеФормыКоллекция'".to_string(),
        );
        assert_eq!(
            diags,
            [expected.clone(), expected],
            "a value-table-only method on either attribute is still reported, as a missing \
             method of the form collection, got: {diags:?}"
        );

        let server = r#"&НаСервере
Процедура Тест()
    Найденные = Таблица.НайтиСтроки(Новый Структура("Код", 1));
    Значение = РеквизитФормыВЗначение("Таблица");
    Колонка = Значение.ВыгрузитьКолонку("Код");
КонецПроцедуры
"#;
        let diags = form_collection_diags(server);
        assert!(
            diags.is_empty(),
            "РеквизитФормыВЗначение still gives the value table, got: {diags:?}"
        );
    }
}
