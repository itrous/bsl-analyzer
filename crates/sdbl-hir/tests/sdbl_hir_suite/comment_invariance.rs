//! Комментарий в тексте запроса не меняет его смысл.
//!
//! Свойство метаморфное: понижение запроса и понижение того же запроса с
//! дописанным комментарием обязаны дать одинаковый HIR — с точностью до
//! диапазонов, которые сдвигаются вместе с текстом.
//!
//! Проверяются две формы, которыми комментируют запросы в конфигурациях:
//! комментарий отдельной строкой (так отключают условие или соединение) и
//! комментарий в конце строки (так помечают правку). Тексты комментариев
//! подобраны враждебно: в них есть точка, союз `И`, знаки операций и
//! ключевые слова, то есть ровно те подстроки, по которым понижение
//! когда-либо принимало решения.

use sdbl_hir::lower_sdbl_to_hir;

/// Запросы взяты разных форм: то, чем кончается строка перед комментарием,
/// и есть условие срабатывания класса.
const QUERIES: &[&str] = &[
    "ВЫБРАТЬ
	Товары.Артикул КАК Артикул
ИЗ
	Справочник.Товары КАК Товары
ГДЕ
	НЕ Товары.ПометкаУдаления",
    "ВЫБРАТЬ
	Товары.Артикул
ИЗ
	Справочник.Товары КАК Товары
ГДЕ
	Товары.Артикул = &Артикул
	И Товары.Наименование <> \"\"",
    "ВЫБРАТЬ
	Товары.Артикул КАК Артикул,
	СУММА(Товары.Цена) КАК Цена
ИЗ
	Справочник.Товары КАК Товары
СГРУППИРОВАТЬ ПО
	Товары.Артикул
УПОРЯДОЧИТЬ ПО
	Артикул",
    "ВЫБРАТЬ
	Т1.Артикул КАК Артикул
ИЗ
	Справочник.Товары КАК Т1
		ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Товары КАК Т2
		ПО Т1.Артикул = Т2.Артикул
ГДЕ
	Т1.Цена > 100 - 10",
    "ВЫБРАТЬ
	Товары.Артикул КАК Артикул
ИЗ
	Справочник.Товары КАК Товары
ГДЕ
	Товары.Владелец ЕСТЬ НЕ NULL
	И Товары.Артикул В (\"А\", \"Б\")
	И Товары.Цена МЕЖДУ 1 И 100
	И Товары.Наименование ПОДОБНО \"%шуруп%\"",
    "ВЫБРАТЬ
	ВЫБОР
		КОГДА Товары.Цена > 0
			ТОГДА Товары.Цена
		ИНАЧЕ 0
	КОНЕЦ КАК Цена
ИЗ
	Справочник.Товары КАК Товары
ГДЕ
	Товары.Вид = ЗНАЧЕНИЕ(Перечисление.ВидыТоваров.Основной)",
];

/// Каждый текст ломает какой-то один способ принимать решение по подстроке:
/// точка — разбор составного имени, союз и `НЕ` — определение операции,
/// знаки — определение сравнения, ключевое слово — распознавание предиката.
const COMMENTS: &[&str] = &[
    "// комментарий последней строкой",
    "// см. задачу 15",
    "// тут И тут, а ещё НЕ тут",
    "// было <> 0, стало >= 100 - 10",
    "// ЕСТЬ NULL ПОДОБНО В МЕЖДУ",
];

fn lower_to_shape(query: &str) -> String {
    let parse = parser::parse_sdbl(query);
    let package = lower_sdbl_to_hir(&parse, None);
    sorted_range_to_category(&without_ranges(&format!("{:?}", package)))
}

/// `range_to_category` — хеш-таблица по диапазонам, и её порядок обхода
/// меняется от одного сдвига текста. Сравнивать надо состав, а не порядок,
/// иначе свойство падало бы на любой вставке и не значило бы ничего.
fn sorted_range_to_category(debug: &str) -> String {
    const KEY: &str = "range_to_category: {";

    let Some(open) = debug.find(KEY) else {
        return debug.to_string();
    };
    let body_start = open + KEY.len();
    let Some(body_len) = debug[body_start..].find('}') else {
        return debug.to_string();
    };
    let body_end = body_start + body_len;

    let mut entries: Vec<&str> = debug[body_start..body_end].split(", ").collect();
    entries.sort_unstable();

    format!("{}{}{}", &debug[..body_start], entries.join(", "), &debug[body_end..])
}

/// `TextRange` печатается как `12..34`, и только диапазонам позволено
/// разъезжаться: текст комментария сдвигает всё, что стоит за ним.
fn without_ranges(debug: &str) -> String {
    let bytes = debug.as_bytes();
    let mut out = String::with_capacity(debug.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut end = i;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }

            if bytes[end..].starts_with(b"..") {
                let mut tail = end + 2;
                let tail_start = tail;
                while tail < bytes.len() && bytes[tail].is_ascii_digit() {
                    tail += 1;
                }
                if tail > tail_start {
                    out.push_str("<range>");
                    i = tail;
                    continue;
                }
            }

            out.push_str(&debug[start..end]);
            i = end;
            continue;
        }

        let ch = debug[i..].chars().next().expect("i стоит на границе символа");
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

/// Комментарий отдельной строкой после строки `line`.
fn with_own_line_comment(query: &str, line: usize, comment: &str) -> String {
    let mut lines: Vec<String> = query.lines().map(str::to_string).collect();
    lines.insert(line + 1, comment.to_string());
    lines.join("\n")
}

/// Комментарий в конце строки `line`.
fn with_trailing_comment(query: &str, line: usize, comment: &str) -> String {
    let mut lines: Vec<String> = query.lines().map(str::to_string).collect();
    lines[line].push(' ');
    lines[line].push_str(comment);
    lines.join("\n")
}

fn check_all(insert: fn(&str, usize, &str) -> String, form: &str) {
    let mut breaches = Vec::new();

    for query in QUERIES {
        let expected = lower_to_shape(query);
        let line_count = query.lines().count();

        for line in 0..line_count {
            for comment in COMMENTS {
                let commented = insert(query, line, comment);
                let actual = lower_to_shape(&commented);

                if actual != expected {
                    breaches.push(format!(
                        "{form}, строка {line}, {comment}\n--- запрос ---\n{commented}\n--- ожидалось ---\n{expected}\n--- получено ---\n{actual}"
                    ));
                }
            }
        }
    }

    assert!(
        breaches.is_empty(),
        "комментарий изменил смысл запроса в {} случаях:\n\n{}",
        breaches.len(),
        breaches.join("\n\n")
    );
}

#[test]
fn a_comment_on_its_own_line_does_not_change_the_hir() {
    check_all(with_own_line_comment, "отдельной строкой");
}

#[test]
fn a_trailing_comment_does_not_change_the_hir() {
    check_all(with_trailing_comment, "в конце строки");
}

/// Проверка обязана уметь падать: если бы вставка комментария не доходила
/// до понижения, оба свойства были бы зелены при любой реализации.
#[test]
fn the_check_sees_a_real_change_in_the_query() {
    let query = QUERIES[0];
    let renamed = query.replace("ПометкаУдаления", "ПометкаУдаленияX");

    assert_ne!(
        lower_to_shape(query),
        lower_to_shape(&renamed),
        "сравнение слепо к именам полей — свойство ничего не проверяет",
    );
}
