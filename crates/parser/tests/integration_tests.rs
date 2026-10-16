use parser::parse;
use std::time::Instant;
use syntax::SyntaxKind;

fn assert_clean_parse(input: &str, message: &str) {
    let result = parse(input);
    assert!(!result.has_errors(), "{message}");

    let root = result.syntax_node();
    let error_nodes: Vec<_> =
        root.descendants().filter(|node| node.kind() == SyntaxKind::ERROR).collect();
    assert!(error_nodes.is_empty(), "{message}; ERROR nodes: {error_nodes:?}");
}

#[test]
fn test_raise_old_style_string() {
    let input = r#"Процедура Тест()
    ВызватьИсключение "Текст исключения";
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors(), "Should parse old-style raise without errors");
}

#[test]
fn test_raise_call_one_arg() {
    let input = r#"Процедура Тест()
    ВызватьИсключение("Текст ошибки");
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors(), "Should parse raise with one argument without errors");
}

#[test]
fn test_raise_call_multiple_args_with_omitted() {
    let input = r#"Процедура Тест()
    ВызватьИсключение("Текст ошибки", КатегорияОшибки.ОшибкаСети, , ,
                        ФоновоеЗадание.ИнформацияОбОшибке);
КонецПроцедуры"#;
    let result = parse(input);
    assert!(
        !result.has_errors(),
        "Should parse raise with multiple arguments (some omitted) without errors"
    );
}

#[test]
fn test_raise_call_two_args() {
    let input = r#"Процедура Тест()
    ВызватьИсключение("Текст ошибки", КатегорияОшибки.ОшибкаХранимыхДанных);
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors(), "Should parse raise with two arguments without errors");
}

#[test]
fn test_raise_empty() {
    let input = r#"Процедура Тест()
    Попытка
        // code
    Исключение
        ВызватьИсключение;
    КонецПопытки;
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors(), "Should parse empty raise (re-raise) without errors");
}

#[test]
fn test_async_procedure() {
    let input = "Асинх Процедура Тест() КонецПроцедуры";
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_async_function() {
    let input = "Асинх Функция Тест() КонецФункции";
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_compiler_directive() {
    let input = "&НаКлиенте\nПроцедура Тест() КонецПроцедуры";
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_multiple_compiler_directives() {
    let input = "&НаКлиентеНаСервере\nПроцедура Тест() КонецПроцедуры";
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_annotation_without_params() {
    let input = "&До\nПроцедура Тест() КонецПроцедуры";
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_annotation_with_params() {
    let input = r#"&До("Модуль.Метод", Параметр1 = Истина)
Процедура Тест() КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_annotation_does_not_take_another_annotation_as_a_value() {
    let nested = r#"&Перед(&НаКлиенте)
Процедура Тест() КонецПроцедуры"#;
    assert!(
        parse(nested).has_errors(),
        "значением параметра аннотации служит значение, а не другая аннотация"
    );

    let value = r#"&Перед("Тест")
Процедура Тест() КонецПроцедуры"#;
    assert!(!parse(value).has_errors(), "обычное значение параметра принимается");
}

#[test]
fn test_execute_statement() {
    let input = r#"Процедура Тест()
    Выполнить("Сообщить('Привет')");
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_add_handler_statement() {
    let input = r#"Процедура Тест()
    ДобавитьОбработчик Форма.Кнопка.Нажатие, ОбработчикНажатия;
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_remove_handler_statement() {
    let input = r#"Процедура Тест()
    УдалитьОбработчик Форма.Кнопка.Нажатие, ОбработчикНажатия;
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_await_expression() {
    let input = r#"Асинх Функция Тест()
    Результат = Ждать ВыполнитьАсинх();
    Возврат Результат;
КонецФункции"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_multiline_string() {
    let input = r#"Процедура Тест()
    Текст = "Строка1
    |Строка2
    |Строка3";
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_complex_procedure() {
    let input = r#"&НаКлиентеНаСервере
&После("Модуль.Метод")
Асинх Процедура СложнаяПроцедура(Параметр1, Знач Параметр2 = 10) Экспорт
    Перем Локальная;

    Локальная = Ждать АсинхОперация();

    Если Локальная > 0 Тогда
        Выполнить("Сообщить('OK')");
    КонецЕсли;

    ДобавитьОбработчик Событие, Обработчик;
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_all_statement_types() {
    let input = r#"Процедура Тест()
    // Присваивание
    А = 10;

    // Вызов
    Сообщить("Привет");

    // Возврат
    Возврат А;

    // Если
    Если А > 5 Тогда
        Б = 20;
    КонецЕсли;

    // Пока
    Пока А < 100 Цикл
        А = А + 1;
    КонецЦикла;

    // Для
    Для Счетчик = 1 По 10 Цикл
        А = А + Счетчик;
    КонецЦикла;

    // Для Каждого
    Для Каждого Элемент Из Массив Цикл
        Сообщить(Элемент);
    КонецЦикла;

    // Попытка
    Попытка
        ОпаснаяОперация();
    Исключение
        Сообщить("Ошибка");
    КонецПопытки;

    // ВызватьИсключение
    ВызватьИсключение "Ошибка";

    // Прервать
    Прервать;

    // Продолжить
    Продолжить;

    // Перейти
    ~Метка:
    Перейти ~Метка;

    // Выполнить
    Выполнить("Код");

    // ДобавитьОбработчик
    ДобавитьОбработчик Событие, Обработчик;

    // УдалитьОбработчик
    УдалитьОбработчик Событие, Обработчик;
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_all_expression_types() {
    let input = r#"Функция Тест()
    // Литералы
    А = 10;
    Б = 3.14;
    В = "Строка";
    Г = '20231231';
    Д = Истина;
    Е = Ложь;
    Ж = Неопределено;
    З = Null;

    // Арифметические операции
    И1 = А + Б;
    К = А - Б;
    Л = А * Б;
    М = А / Б;
    Н = А % Б;

    // Логические операции
    О = А > Б;
    П = А < Б;
    Р = А >= Б;
    С = А <= Б;
    Т = А = Б;
    У = А <> Б;
    Ф = Д И Е;
    Х = Д ИЛИ Е;
    Ц = НЕ Д;

    // Вызовы
    Ч = ФункцияБезПараметров();
    Ш = ФункцияСПараметрами(1, 2, 3);
    Щ = Объект.Метод();

    // Доступ к свойствам
    Ы = Объект.Свойство;
    Э = Объект.Свойство1.Свойство2;

    // Индексация
    Ю = Массив[0];
    Я = Массив[Индекс];

    // New
    А1 = Новый Массив;
    Б1 = Новый Структура("Ключ", Значение);

    // Тернарный оператор
    В1 = ?(Условие, ЗначениеИстина, ЗначениеЛожь);

    // Await
    Г1 = Ждать АсинхФункция();

    Возврат Г1;
КонецФункции"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_simple_platform_symbol() {
    let input = r#"#Если Клиент Тогда
    Процедура ТестНаКлиенте() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_and_operator() {
    let input = r#"#Если Клиент И НЕ Сервер Тогда
    Процедура Тест() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_or_operator() {
    let input = r#"#Если ВебКлиент ИЛИ МобильныйКлиент Тогда
    Процедура Тест() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_complex_expression() {
    let input = r#"#Если (Клиент И НЕ МобильныйКлиент) ИЛИ (Сервер И Windows) Тогда
    Процедура Тест() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_elsif_else() {
    let input = r#"#Если Клиент Тогда
    Процедура НаКлиенте() КонецПроцедуры
#ИначеЕсли Сервер Тогда
    Процедура НаСервере() КонецПроцедуры
#Иначе
    Процедура Общая() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_with_space_after_hash_inside_procedure() {
    let input = r#"Процедура Тест()
    # Если ВебКлиент Тогда
        Возврат;
    # Иначе
        Сообщить("Не веб");
    # КонецЕсли
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors(), "Should parse spaced preprocessor directives without errors");
}

#[test]
fn test_preprocessor_parenthesized_operands_with_and() {
    let input = r#"Процедура Тест()
#Если (Не ВебКлиент) И (Не МобильныйКлиент) Тогда
    ИмяАрхива = "stat.zip";
#КонецЕсли
КонецПроцедуры"#;
    assert_clean_parse(
        input,
        "Should parse preprocessor boolean expression after a parenthesized operand",
    );
}

#[test]
fn test_preprocessor_multiple_elsif() {
    let input = r#"#Если ТонкийКлиент Тогда
    Процедура Тонкий() КонецПроцедуры
#ИначеЕсли ВебКлиент Тогда
    Процедура Веб() КонецПроцедуры
#ИначеЕсли МобильныйКлиент Тогда
    Процедура Мобильный() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_not_expression() {
    let input = r#"#Если НЕ (Клиент ИЛИ Сервер) Тогда
    Процедура Тест() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_unknown_symbol_still_parses() {
    // Разбор не зависит от того, известен ли символ: список известных живёт
    // в диагностике, а грамматика принимает в условии любой идентификатор.
    let input = r#"#Если Мираж Тогда
    Процедура ПриМираже() КонецПроцедуры
#ИначеЕсли НЕ Морок Тогда
    Процедура ПриМороке() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_all_platform_symbols() {
    let input = r#"#Если ТолстыйКлиентОбычноеПриложение Тогда
    Процедура Тест1() КонецПроцедуры
#ИначеЕсли ТолстыйКлиентУправляемоеПриложение Тогда
    Процедура Тест2() КонецПроцедуры
#ИначеЕсли МобильноеПриложениеКлиент Тогда
    Процедура Тест3() КонецПроцедуры
#ИначеЕсли МобильноеПриложениеСервер Тогда
    Процедура Тест4() КонецПроцедуры
#ИначеЕсли ВнешнееСоединение Тогда
    Процедура Тест5() КонецПроцедуры
#КонецЕсли"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_iso_date_literal_in_return_expression() {
    let input = r#"Функция МинимальнаяДата()
    Возврат '0001-01-01';
КонецФункции"#;
    let result = parse(input);
    assert!(!result.has_errors(), "Should parse ISO date literal without errors");
}

#[test]
fn test_dotted_and_comma_date_literals_in_expression() {
    let input = r#"Процедура Тест()
    Начало = '1000.01.01 00:00.00';
    Конец = '2099.12.31 23:59.59';
    Минимальная = Дата('0001,01,01');
КонецПроцедуры"#;
    assert_clean_parse(input, "Should parse dotted and comma-separated date literals");
}

#[test]
fn test_trailing_dot_numeric_literal_in_condition() {
    let input = r#"Процедура Тест(Значение)
    Если Значение < 0. Тогда
        Возврат;
    КонецЕсли;
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors(), "Should parse numeric literal with trailing dot without errors");
}

#[test]
fn test_chained_comparisons_in_conditions_and_assignments() {
    let input = r#"Процедура Тест(Значение, Блок)
    Если 60 < Значение <= 3600 Тогда
        Возврат;
    КонецЕсли;
    Если ВидСКД <> "Форма" <> НомерРаздела = "02" Тогда
        Возврат;
    КонецЕсли;
    Значение1 = Блок[0] <> Блок[2] <> Блок[4];
КонецПроцедуры"#;
    assert_clean_parse(input, "Should parse chained comparison operators");
}

#[test]
fn test_bare_raise_before_block_end() {
    let input = r#"Процедура Тест()
    Попытка
        Действие();
    Исключение
        ВызватьИсключение
    КонецПопытки;
КонецПроцедуры"#;
    assert_clean_parse(input, "Should parse bare raise without semicolon before КонецПопытки");
}

#[test]
fn property_access_after_constructor_requires_a_temporary() {
    assert_clean_parse(
        "Процедура Тест()\n    Файл = Новый Файл(\"a\");\n    Имя = Файл.ПолноеИмя;\n    Есть = Файл.Существует();\nКонецПроцедуры",
        "member access through a temporary constructed value is valid",
    );
}

#[test]
fn test_adjacent_string_literals() {
    let input = r#"Процедура Тест()
    Данные.Вставить("Ключ" "");
    Расшифровка = """" + ЛеваяЧасть + """" + " = " + ?(Истина, """" """", """" + ПраваяЧасть + """");
КонецПроцедуры"#;
    assert_clean_parse(input, "Should parse adjacent string literals as one string expression");
}

#[test]
fn test_multiline_string_without_bar_continuation() {
    let input = r#"Процедура Тест()
    ТекстПодсказки = НСтр("ru = 'Доплата может производиться картой,
        "а также наличными.'");
КонецПроцедуры"#;
    assert_clean_parse(input, "Should parse multiline string continuation without leading bar");
}

#[test]
fn test_nbsp_as_whitespace_in_bsl_code() {
    let input = "Процедура Тест()\n\u{00A0}\u{00A0}\u{00A0}\u{00A0}А = 1;\u{00A0}\nКонецПроцедуры";
    assert_clean_parse(input, "Should parse NBSP as whitespace");
}

#[test]
fn test_metadata_access() {
    let input = r#"Функция Тест() Экспорт
    Ссылка = Справочник.Номенклатура.НайтиПоКоду("001");
    Метаданные = Ссылка.Метаданные().Имя;
    Документ = Документы.ПоступлениеТоваровИУслуг.СоздатьДокумент();
    Возврат Метаданные;
КонецФункции"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_built_in_functions() {
    let input = r#"Функция Тест() Экспорт
    СтрРазделитель = Символы.ПС;
    Результат = СтрЗаменить("Строка", "о", "а");
    Массив = СтроковыеФункцииКлиентСервер.РазложитьСтрокуВМассивПодстрок(Результат, СтрРазделитель, Истина);
    Структура = Новый Структура("Ключ, Значение", "Тест", 123);
    Возврат Структура;
КонецФункции"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_real_bsl_code_sample() {
    let input = r#"#Область ПрограммныйИнтерфейс

Функция ЗначениеНастройкиПланаОбмена(ИмяПланаОбмена, ИмяПараметра, ИдентификаторНастройки = "", ВерсияКорреспондента = "") Экспорт
	ЗначениеПараметра = Новый Структура;
	НастройкиПланаОбмена = Неопределено;
	ИменаПараметров = СтроковыеФункцииКлиентСервер.РазложитьСтрокуВМассивПодстрок(ИмяПараметра,,Истина);

	Если ИменаПараметров.Количество() = 0 Тогда
		Возврат Неопределено;
	КонецЕсли;

	Для Каждого ЕдиничныйПараметр Из ИменаПараметров Цикл
		ЗначениеЕдиничногоПараметра = Неопределено;
		Если ИменаПараметров.Количество() = 1 Тогда
			Возврат ЗначениеЕдиничногоПараметра;
		Иначе
			ЗначениеПараметра.Вставить(ЕдиничныйПараметр, ЗначениеЕдиничногоПараметра);
		КонецЕсли;
	КонецЦикла;

	Возврат ЗначениеПараметра;
КонецФункции

#КонецОбласти"#;
    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_region_minimal() {
    let input = r#"#Область Test
Функция Тест()
КонецФункции
#КонецОбласти"#;

    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_not_without_parens() {
    let input = r#"#Если НЕ Клиент Тогда
    Процедура Тест() КонецПроцедуры
#КонецЕсли"#;

    let result = parse(input);
    assert!(!result.has_errors());
}

#[test]
fn test_preprocessor_if_torn_into_expression_keeps_enclosing_region() {
    // A directive inside a condition must not leave its `#КонецЕсли` to the
    // region around it: that would close the module-level region before the
    // procedure that follows.
    let input = r#"#Если Не ТонкийКлиент И Не ВебКлиент Тогда
Функция Первая()
    Если Ложь
        #Если ТолстыйКлиентУправляемоеПриложение Тогда
        Или Ложь
        #КонецЕсли
    Тогда
        Возврат 1;
    КонецЕсли;
    Возврат 0;
КонецФункции

Процедура Вторая()
КонецПроцедуры
#КонецЕсли
"#;
    let parsed = parse(input);
    assert!(!parsed.has_errors(), "{:?}", parsed.errors());
    let root = parsed.syntax_node();
    let regions: Vec<_> = root.children().filter(|n| n.kind() == SyntaxKind::PRE_IF_DIR).collect();
    assert_eq!(regions.len(), 1, "one module-level region: {regions:?}");
    let region = &regions[0];
    assert_eq!(
        region.text().to_string().trim_end(),
        input.trim_end(),
        "the region must end at the last #КонецЕсли"
    );
    assert!(
        region.descendants().any(|n| n.kind() == SyntaxKind::PROCEDURE_DEF),
        "the procedure after the torn condition stays inside the region"
    );
    // The directive in the condition tears it apart and is read as one
    // variant: its lines are opaque parts around the branch read, not a block.
    let nested = region.descendants().filter(|n| n.kind() == SyntaxKind::PRE_IF_DIR).count() - 1;
    assert_eq!(nested, 0, "the directive in the condition is not a structured block");
    let parts = region.descendants().filter(|n| n.kind() == SyntaxKind::PRE_TORN_IF_PART).count();
    assert_eq!(parts, 2, "its `#Если` line and its `#КонецЕсли` line");
}

#[test]
fn test_preprocessor_if_crossing_an_else_if_keeps_enclosing_region() {
    // The block covers an `ИначеЕсли` clause of the `Если` it was opened in:
    // its `#КонецЕсли` is its own, not the module-level region's.
    let input = r#"#Если Не ТонкийКлиент И Не ВебКлиент Тогда
Процедура Первая(А)
    Если А = 1 Тогда
        Сообщить("1");
    #Если Клиент Тогда
    ИначеЕсли А = 2 Тогда
        Сообщить("2");
    #Иначе
    ИначеЕсли А = 3 Тогда
        Сообщить("3");
    #КонецЕсли
    КонецЕсли;
КонецПроцедуры

Процедура Вторая()
КонецПроцедуры
#КонецЕсли
"#;
    let parse = parse(input);
    let root = parse.syntax_node();
    let regions: Vec<_> = root.children().filter(|n| n.kind() == SyntaxKind::PRE_IF_DIR).collect();
    assert_eq!(regions.len(), 1, "one module-level region: {regions:?}");
    assert_eq!(
        regions[0].text().to_string().trim_end(),
        input.trim_end(),
        "the region must end at the last #КонецЕсли"
    );
    let procedures =
        regions[0].descendants().filter(|n| n.kind() == SyntaxKind::PROCEDURE_DEF).count();
    assert_eq!(procedures, 2, "both procedures stay inside the region");
    let if_stmts: Vec<_> = root.descendants().filter(|n| n.kind() == SyntaxKind::IF_STMT).collect();
    assert_eq!(if_stmts.len(), 1, "{if_stmts:?}");
    assert!(
        if_stmts[0].text().to_string().trim_end().ends_with("КонецЕсли;"),
        "the statement keeps its own closer: {}",
        if_stmts[0].text()
    );
}

#[test]
fn test_small_file_performance() {
    let input = r#"Функция Тест() Экспорт
    Результат = 10 + 20;
    Возврат Результат;
КонецФункции"#;

    let start = Instant::now();
    let iterations = 1000;

    for _ in 0..iterations {
        let result = parse(input);
        assert!(!result.has_errors());
    }

    let elapsed = start.elapsed();
    let avg_micros = elapsed.as_micros() / iterations;

    println!("\nSmall file performance:");
    println!("Iterations: {}", iterations);
    println!("Total time: {:?}", elapsed);
    println!("Average per parse: {} μs", avg_micros);

    assert!(avg_micros < 1000, "Parser too slow even for simple code: {} μs", avg_micros);
}

#[test]
fn test_lexer_performance_large_file() {
    use lexer::tokenize;

    let input = include_str!("fixtures/Module.bsl");

    println!("\nLexer performance test:");
    println!("File size: {} bytes", input.len());

    let start = Instant::now();
    let tokens = tokenize(input);
    let elapsed = start.elapsed();

    println!("Tokens: {}", tokens.len());
    println!("Lexer time: {:?}", elapsed);
    println!("Speed: {:.2} MB/s", input.len() as f64 / 1_000_000.0 / elapsed.as_secs_f64());

    // Threshold is load-tolerant: it catches order-of-magnitude regressions while
    // tolerating the parallel load of a full-workspace `cargo test --all` run.
    assert!(elapsed.as_millis() < 500, "Lexer too slow: {:?}", elapsed);
}

#[test]
fn benchmark_parser_performance() {
    let input = include_str!("fixtures/Module.bsl");

    let file_size_bytes = input.len();
    let file_size_mb = file_size_bytes as f64 / 1_000_000.0;

    println!("\n=== Parser Performance Benchmark ===");
    println!("File: fixtures/Module.bsl");
    println!("Size: {} bytes ({:.2} MB)", file_size_bytes, file_size_mb);

    println!("\nParsing file (this may take time)...");
    let start = Instant::now();

    let result = parse(input);
    assert!(!result.has_errors());

    let elapsed = start.elapsed();
    let throughput = file_size_mb / elapsed.as_secs_f64();

    println!("\nResults:");
    println!("Parse time: {:.2?}", elapsed);
    println!("Throughput: {:.2} MB/s", throughput);

    println!("\n=== Criterion Check ===");
    if throughput >= 50.0 {
        println!("✅ PASS: Throughput {:.2} MB/s >= 50 MB/s target", throughput);
    } else {
        println!("⚠️  NOTICE: Throughput {:.2} MB/s < 50 MB/s target", throughput);
        println!("Parser works but needs optimization");
    }
}
#[test]
fn debug_tokens_around_1905() {
    use lexer::tokenize;
    let input = include_str!("fixtures/Module.bsl");

    let tokens = tokenize(input);

    println!("\nTokens around position 1905:");
    for i in 1895..1915 {
        if i < tokens.len() {
            println!("{:4}: {:?}", i, tokens[i].kind);
        }
    }

    println!("\nToken 1905 is: {:?}", tokens.get(1905));
}
#[test]
fn debug_tokens_around_14467() {
    use lexer::tokenize;
    let input = include_str!("fixtures/Module.bsl");

    let tokens = tokenize(input);

    println!("\nTokens around position 14467:");
    for i in 14457..14477 {
        if i < tokens.len() {
            println!("{:5}: {:?}", i, tokens[i].kind);
        }
    }
}

#[test]
fn debug_tokens_around_68041() {
    use lexer::tokenize;
    let input = include_str!("fixtures/Module.bsl");

    let tokens = tokenize(input);

    println!("\nTotal tokens: {}", tokens.len());
    println!("\nTokens around position 68041 (wider context):");
    for i in 68000..68060 {
        if i < tokens.len() {
            println!("{:5}: {:?}", i, tokens[i].kind);
        }
    }

    println!("\nToken 68041 is: {:?}", tokens.get(68041));
}

#[test]
fn test_keyword_as_method_name() {
    let input = r#"Процедура Тест()
    Поток.Перейти(0, ПозицияВПотоке.Начало);
КонецПроцедуры"#;
    let result = parse(input);
    assert!(!result.has_errors(), "Keyword 'Перейти' should be valid as method name after dot");

    let error_nodes: Vec<_> = result
        .syntax_node()
        .descendants()
        .filter(|node| node.kind().to_string() == "ERROR" && !node.text_range().is_empty())
        .collect();
    assert!(
        error_nodes.is_empty(),
        "Should have no ERROR nodes, found: {:?}",
        error_nodes.iter().map(|n| n.text().to_string()).collect::<Vec<_>>()
    );
}

#[test]
fn test_partial_dot_preserves_enclosing_end_function() {
    let input = "Функция A()\n    Х = B();\n    Х.\nКонецФункции\n\nФункция B()\n    Возврат 1;\nКонецФункции\n";
    let result = parse(input);

    let errs = result.errors();
    assert!(
        errs.iter().any(|e| e.message().contains("свойства")),
        "expected `ожидалось имя свойства` error on partial dot; got: {:?}",
        errs.iter().map(|e| e.message().to_string()).collect::<Vec<_>>(),
    );

    let function_defs: Vec<_> = result
        .syntax_node()
        .descendants()
        .filter(|n| n.kind().to_string() == "FUNCTION_DEF")
        .collect();
    assert_eq!(
        function_defs.len(),
        2,
        "partial dot must leave both function declarations intact; got {} FUNCTION_DEFs",
        function_defs.len()
    );
}

#[test]
fn test_partial_dot_preserves_next_function_declaration() {
    let input = "Функция Caller()\n    X.\nКонецФункции\n\nФункция Callee()\n    Возврат 0;\nКонецФункции\n";
    let result = parse(input);
    let names: Vec<String> = result
        .syntax_node()
        .descendants()
        .filter(|n| n.kind().to_string() == "FUNCTION_DEF")
        .filter_map(|fn_def| {
            fn_def.descendants_with_tokens().find_map(|el| {
                el.into_token()
                    .filter(|t| t.kind().to_string() == "IDENT")
                    .map(|t| t.text().to_string())
            })
        })
        .collect();
    assert_eq!(
        names,
        vec!["Caller".to_string(), "Callee".to_string()],
        "both declarations must be reachable through FUNCTION_DEF nodes"
    );
}

#[test]
fn test_partial_dot_preserves_orphaned_function_declaration() {
    let input = "Функция Caller()\n    X.\nФункция Callee()\n    Возврат 0;\nКонецФункции\n";
    let result = parse(input);

    let errs = result.errors();
    assert!(
        errs.iter().any(|e| e.message().contains("свойства")),
        "expected `ожидалось имя свойства` error on partial dot; got: {:?}",
        errs.iter().map(|e| e.message().to_string()).collect::<Vec<_>>(),
    );
    assert!(
        result.syntax_node().descendants().any(|n| {
            n.kind().to_string() == "FUNCTION_DEF"
                && n.descendants_with_tokens().any(|el| {
                    el.into_token()
                        .is_some_and(|t| t.kind().to_string() == "IDENT" && t.text() == "Callee")
                })
        }),
        "orphaned `Функция Callee()` declaration must not be swallowed as a property name"
    );
}

#[test]
fn test_large_file_performance() {
    let input = include_str!("fixtures/Module.bsl");

    println!("\nLarge file performance:");
    println!("File size: {} bytes ({:.2} MB)", input.len(), input.len() as f64 / 1_048_576.0);

    let start = Instant::now();
    let result = parse(input);
    let elapsed = start.elapsed();

    println!("Parse time: {:?}", elapsed);
    println!("Performance: {:.2} MB/s", (input.len() as f64 / 1_048_576.0) / elapsed.as_secs_f64());

    assert!(!result.has_errors());
}

#[test]
fn parser_does_not_panic_on_large_xml_like_input() {
    let mut input = String::with_capacity(5_000_000);
    input.push_str("<?xml version=\"1.0\"?>\n<root>\n");
    for i in 0..100_000 {
        input.push_str(&format!("  <item id=\"{}\"><name>Value{}</name></item>\n", i, i));
    }
    input.push_str("</root>\n");
    assert!(input.len() > 4_000_000, "regression fixture must exceed 4 MB");

    let t0 = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parse(&input)));
    let elapsed = t0.elapsed();
    assert!(
        result.is_ok(),
        "parser panicked on {}-byte XML-like input (elapsed {:?}): {:?}",
        input.len(),
        elapsed,
        result.err().and_then(|p| p.downcast_ref::<String>().cloned())
    );
}

#[test]
fn parser_handles_million_token_valid_bsl() {
    let mut input = String::with_capacity(2_000_000);
    for i in 0..50_000 {
        input.push_str(&format!("Процедура Proc{i}()\n    Сообщить({i});\nКонецПроцедуры\n"));
    }

    let t0 = Instant::now();
    let result = parse(&input);
    let elapsed = t0.elapsed();
    assert!(
        !result.has_errors(),
        "valid BSL regression fixture produced errors ({:?}, {} bytes): {:?}",
        elapsed,
        input.len(),
        result.errors()
    );
}

#[test]
fn parser_guard_panics_on_stuck_loop() {
    use lexer::tokenize;
    use parser::Parser;

    let tokens = tokenize("Процедура Т() КонецПроцедуры");
    let mut p = Parser::new(&tokens);
    let hit_guard = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loop {
        p.check_iteration_limit();
    }));
    let panic_msg = match hit_guard {
        Ok(()) => panic!("stuck parser guard did not panic"),
        Err(payload) => payload
            .downcast_ref::<&'static str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default(),
    };
    assert!(
        panic_msg.contains("STUCK"),
        "expected STUCK diagnostic in guard panic, got: {panic_msg}"
    );
}

mod keyword_member_names {
    use parser::parse;
    use syntax::ast_utils::field_tail_name_token;
    use syntax::SyntaxKind;

    fn first_field_member(node: &syntax::SyntaxNode) -> Option<String> {
        node.descendants()
            .find(|n| n.kind() == SyntaxKind::FIELD_EXPR)
            .and_then(|fe| field_tail_name_token(&fe))
            .map(|tok| tok.text().to_string())
    }

    #[test]
    fn same_line_keyword_members_parse_clean() {
        // Real ERP patterns: BSL keywords used as member / enum-value names after '.'.
        let input = r#"Процедура Тест()
    ВСД.for = "id";
    Для Каждого Ответ Из XDTOРезультат.return Цикл
    КонецЦикла;
    Результат = РассчитанноеУсловие(Условие.Иначе);
    Значение = Перечисления.ТребованияКПодписаниюЭД.ИЛИ;
    ТекущийЗапрос.Попытка = ТекущийЗапрос.Попытка + 1;
КонецПроцедуры"#;
        let result = parse(input);
        assert!(!result.has_errors(), "keyword member names must parse without errors");
        let errors: Vec<_> =
            result.syntax_node().descendants().filter(|n| n.kind() == SyntaxKind::ERROR).collect();
        assert!(errors.is_empty(), "no ERROR nodes expected, got: {errors:?}");
    }

    #[test]
    fn noun_like_keyword_members_parse_clean() {
        // Noun-like keywords are real field names in ERP and must be accepted as members.
        let input = "Процедура Т()\n\tЕсли Не ПустаяСтрока(Выборка.Исключение) Тогда\n\t\tЭлементы.Исключение.Видимость = Ложь;\n\t\tСтрокаЗадачи.Цикл = ЗадачаXDTO.iterationNumber;\n\tКонецЕсли;\nКонецПроцедуры";
        assert!(
            !parse(input).has_errors(),
            "Выборка.Исключение / Элементы.Исключение / СтрокаЗадачи.Цикл must parse clean"
        );
    }

    #[test]
    fn fluent_chain_keyword_member_parses_clean() {
        // Member on the line after the receiver (dot starts the line) — common chain style.
        let input = "Процедура Тест()\n\tЗначение = Запрос\n\t\t.Попытка;\nКонецПроцедуры";
        assert!(!parse(input).has_errors(), "fluent-chain keyword member must parse clean");
    }

    #[test]
    fn cross_newline_does_not_swallow_block_closer() {
        // Dangling dot at end of line: a block-closing keyword on the next line must
        // NOT be eaten as a member, so the enclosing block stays intact (recovery).
        let input = "Функция A()\n\tX.\nКонецФункции\n\nФункция B()\n\tВозврат 1;\nКонецФункции\n";
        let parse = parse(input);
        assert!(parse.has_errors(), "dangling dot should report an error");
        let fn_defs = parse
            .syntax_node()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::FUNCTION_DEF)
            .count();
        assert_eq!(fn_defs, 2, "both functions must survive; КонецФункции must not be swallowed");
    }

    #[test]
    fn keyword_member_name_is_extracted() {
        let parse = parse("Процедура Т()\n\tВСД.for = 1;\nКонецПроцедуры");
        let name = first_field_member(&parse.syntax_node());
        assert_eq!(name.as_deref(), Some("for"), "keyword member text must be preserved");
    }

    #[test]
    fn same_line_dangling_dot_recovery_is_bounded() {
        // Any keyword after a dot is a member (BSL keywords aren't reserved as field
        // names — real ERP code has Выборка.Исключение, СтрокаЗадачи.Цикл, etc.), so a
        // same-line dangling dot before a control keyword does report a local error,
        // but it must NOT cascade past the enclosing procedure.
        let input = "Процедура Т()\n\tЕсли Объект. Тогда\n\t\tХ = 1;\n\tКонецЕсли;\nКонецПроцедуры\n\nПроцедура Вторая()\n\tВозврат;\nКонецПроцедуры";
        let parse = parse(input);
        assert!(parse.has_errors(), "same-line dangling dot should report an error");
        let procs = parse
            .syntax_node()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::PROCEDURE_DEF)
            .count();
        assert_eq!(procs, 2, "error must stay local — the second procedure must still be parsed");
    }

    #[test]
    fn eof_after_dot_errors_cleanly() {
        let parse = parse("Процедура Т()\n\tЗначение = Объект.\nКонецПроцедуры");
        assert!(parse.has_errors(), "dangling dot before КонецПроцедуры must error");
        assert!(
            parse.syntax_node().descendants().any(|n| n.kind() == SyntaxKind::PROCEDURE_DEF),
            "the procedure must still close (КонецПроцедуры not swallowed)"
        );
    }

    #[test]
    fn dangling_dot_before_declaration_still_recovers() {
        // Orphaned-declaration guard: a dangling dot must NOT swallow the next method.
        let input = "Результат = Объект.\nПроцедура Вторая()\nКонецПроцедуры";
        let parse = parse(input);
        assert!(parse.has_errors(), "dangling dot before a declaration should report an error");
        let has_proc =
            parse.syntax_node().descendants().any(|n| n.kind() == SyntaxKind::PROCEDURE_DEF);
        assert!(has_proc, "the following Процедура must be recovered as a declaration, not eaten");
    }
}

mod region_directive_bounds {
    use super::*;

    fn region_dir_texts(input: &str) -> Vec<String> {
        parse(input)
            .syntax_node()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::PRE_REGION_DIR)
            .map(|n| n.text().to_string())
            .collect()
    }

    /// A region name lives on the directive's own line. Reaching past the newline
    /// steals the next statement's identifier, leaving that statement headless.
    #[test]
    fn nameless_region_does_not_swallow_the_next_line() {
        let texts = region_dir_texts("#Область\nИмя = 1;\n#КонецОбласти\n");
        assert_eq!(texts[0], "#Область", "directive must end at its own line");

        assert_clean_parse(
            "#Область Р\nИмя = 1;\n#КонецОбласти\n",
            "a named region followed by an assignment must parse cleanly",
        );
    }

    #[test]
    fn nameless_region_does_not_swallow_across_a_comment() {
        let texts = region_dir_texts("#Область\n// хвост\nИмя = 1;\n");
        assert_eq!(texts[0], "#Область");
    }

    #[test]
    fn name_is_still_taken_from_the_directive_line() {
        assert_eq!(region_dir_texts("#Область Р\n")[0], "#Область Р");
        assert_eq!(region_dir_texts("#Область   Р\n")[0], "#Область   Р");
    }
}
