//! Field continuations belong to the documented container, not its return-type union.

use super::{
    mark_role, parse_parameter_line, parse_parameter_types, parse_return_type_union,
    parse_type_line, split_type_description, DescriptionUse, LineRole, MapEntryDoc, ParameterDoc,
    TypeDoc,
};

/// Consumes a field block while retaining the boundary with the next top-level declaration.
/// Returns the ordinary fields and the quoted map key items of the block.
pub(super) fn parse_fields(
    lines: &[String],
    cursor: &mut usize,
    roles: Option<&mut [LineRole]>,
) -> (Vec<ParameterDoc>, Vec<MapEntryDoc>) {
    let Some(level) = lines.get(*cursor).and_then(|line| marker_depth(line.trim())) else {
        return (Vec::new(), Vec::new());
    };
    parse_fields_at_level(lines, cursor, level, roles)
}

/// The number of leading stars is the field's nesting level in 1C documentation.
fn parse_fields_at_level(
    lines: &[String],
    cursor: &mut usize,
    level: usize,
    mut roles: Option<&mut [LineRole]>,
) -> (Vec<ParameterDoc>, Vec<MapEntryDoc>) {
    let mut fields = Vec::<ParameterDoc>::new();
    let mut entries = Vec::<MapEntryDoc>::new();
    let mut current: Option<usize> = None;
    let mut field_indent = indentation(&lines[*cursor]);
    let mut open_union = false;

    while let Some(line) = lines.get(*cursor) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            *cursor += 1;
            continue;
        }
        if let Some(marker_level) = marker_depth(trimmed) {
            if marker_level < level {
                break;
            }
            if marker_level > level {
                let (nested, nested_entries) =
                    parse_fields_at_level(lines, cursor, marker_level, roles.as_deref_mut());
                if let Some(field) = current.map(|index| &mut fields[index]) {
                    if let Some(last_type) = field.types.last_mut() {
                        last_type.parameters.extend(nested);
                        last_type.map_entries.extend(nested_entries);
                    }
                }
                continue;
            }

            mark_role(&mut roles, *cursor, LineRole::FieldMarker(level));
            let item = trimmed.get(level..).map(str::trim).unwrap_or_default();
            current = if item.starts_with('"') {
                // A key item is one line: what follows it is neither its continuation nor nested
                // under it.
                entries.extend(parse_map_entry(item));
                None
            } else {
                parse_field(trimmed, level).map(|field| {
                    fields.push(field);
                    fields.len() - 1
                })
            };
            field_indent = indentation(line);
            open_union = field_union_is_open(trimmed);
        } else if indentation(line) > field_indent {
            let mut role = LineRole::Text { level, description: DescriptionUse::Prose };
            if let Some(field) = current.map(|index| &mut fields[index]) {
                let continuation = (open_union || trimmed.starts_with('-'))
                    .then(|| continuation_types(trimmed))
                    .flatten();
                if let Some(types) = continuation {
                    field.types.extend(types);
                    open_union = type_union_is_open(trimmed);
                    role = LineRole::Type(level);
                } else {
                    if let Some(last) = field.types.last_mut() {
                        let description = last.description.get_or_insert_with(String::new);
                        if !description.is_empty() {
                            description.push('\n');
                        }
                        description.push_str(trimmed);
                    }
                    open_union = false;
                    let description = DescriptionUse::of(field.types.last());
                    role = LineRole::Text { level, description };
                }
            }
            mark_role(&mut roles, *cursor, role);
        } else {
            break;
        }
        *cursor += 1;
    }
    (fields, entries)
}

/// The same field marker must be recognized by hover and semantic highlighting.
pub(super) fn marker_depth(line: &str) -> Option<usize> {
    let depth = line.bytes().take_while(|byte| *byte == b'*').count();
    (depth > 0).then_some(depth)
}

/// Field names and type slots use the ordinary parameter grammar after the marker.
fn parse_field(line: &str, level: usize) -> Option<ParameterDoc> {
    let (name, types) = parse_parameter_line(line.get(level..)?.trim())?;
    Some(ParameterDoc { name, types })
}

/// `"ключ" - Тип - описание`: a BSL string literal as the key, then the ordinary type grammar.
///
/// The key ends at the first single closing quote (`""` inside is an escaped quote), so dashes,
/// spaces and ` - ` inside the quotes belong to the key and never to the separator.
fn parse_map_entry(item: &str) -> Option<MapEntryDoc> {
    let mut chars = item.char_indices().skip(1).peekable();
    let close = loop {
        let (index, character) = chars.next()?;
        if character != '"' {
            continue;
        }
        if chars.peek().is_some_and(|(_, next)| *next == '"') {
            chars.next();
            continue;
        }
        break index;
    };
    let key = crate::extension_merge::unquote_bsl_string(&item[..=close]);
    let after_dash = item[close + 1..].trim_start().strip_prefix('-')?;
    if !after_dash.starts_with(char::is_whitespace) {
        return None;
    }
    Some(MapEntryDoc { key, types: parse_parameter_types(after_dash.trim_start()) })
}

/// Tabs align documentation columns; comparing their visual indentation keeps fields grouped.
pub(super) fn indentation(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).fold(0, |column, character| {
        if character == '\t' {
            (column / 4 + 1) * 4
        } else {
            column + 1
        }
    })
}

/// A trailing comma keeps a field's union open without creating an empty type alternative.
fn continuation_types(line: &str) -> Option<Vec<TypeDoc>> {
    let normalized = line.replace('\t', " ");
    let line = normalized.as_str();
    let line = if split_type_description(line).is_none() {
        line.trim_end_matches(',').trim_end()
    } else {
        line
    };
    parse_return_type_union(line).or_else(|| {
        parse_type_line(line).map(|(name, description)| vec![TypeDoc::simple(name, description)])
    })
}

/// A comma in prose does not introduce another type on the next line.
pub(super) fn type_union_is_open(line: &str) -> bool {
    let normalized = line.replace('\t', " ");
    split_type_description(&normalized)
        .map_or(normalized.as_str(), |(types, _)| types)
        .trim_end()
        .ends_with(',')
}

/// Field entries carry their type slot after the name's separator.
pub(super) fn field_union_is_open(line: &str) -> bool {
    let normalized = line.replace('\t', " ");
    normalized.split_once(" - ").is_some_and(|(_, types)| type_union_is_open(types))
}

#[cfg(test)]
mod tests {
    use super::super::parse_method_docs;

    /// A wrapped description ending in a comma must not add a phantom type to the field.
    #[test]
    fn commas_in_field_descriptions_are_prose() {
        let comments = [
            "Returns: Structure",
            " * Mode - String - allowed values,",
            "   Default - automatic selection",
            " * Count - Number,",
            "   - Undefined - optional,",
            "   Default - leave unchanged",
        ]
        .map(str::to_owned);
        for separator in [" - ", "\t-\t"] {
            let comments: Vec<_> =
                comments.iter().map(|line| line.replace(" - ", separator)).collect();
            let docs = parse_method_docs(&comments).unwrap();
            let fields = &docs.returned_value[0].parameters;
            assert_eq!(fields[0].types.len(), 1);
            assert_eq!(
                fields[0].types[0].description.as_deref(),
                Some(format!("allowed values,\nDefault{separator}automatic selection").as_str())
            );
            assert_eq!(fields[1].types.len(), 2);
            assert_eq!(
                fields[1].types[1].description.as_deref(),
                Some(format!("optional,\nDefault{separator}leave unchanged").as_str())
            );
        }
    }

    /// Aligned continuation types must stay on their field instead of leaking into returns.
    #[test]
    fn return_fields_keep_multiline_unions_and_descriptions() {
        let comments = [
            "Возвращаемое значение:",
            "  Структура - параметры документа:",
            "    * ВидОперации - ПеречислениеСсылка.Поступление,",
            "                   ПеречислениеСсылка.Возврат - вид операции.",
            "    * Документ - ДокументСсылка.Поступление,",
            "               - ДокументСсылка.Списание,",
            "               - ДокументСсылка.Возврат - первичный документ.",
            "                 Продолжение описания.",
            "    * Сумма - Число - сумма операции.",
            "  Неопределено - документ не найден.",
        ]
        .map(str::to_owned);
        let docs = parse_method_docs(&comments).unwrap();
        assert_eq!(docs.returned_value.len(), 2);
        assert_eq!(docs.returned_value[1].name, "Неопределено");
        let fields = &docs.returned_value[0].parameters;
        assert_eq!(fields.len(), 3);
        assert_eq!(
            fields[0].types.iter().map(|ty| ty.name.as_str()).collect::<Vec<_>>(),
            ["ПеречислениеСсылка.Поступление", "ПеречислениеСсылка.Возврат"]
        );
        assert_eq!(
            fields[1].types.iter().map(|ty| ty.name.as_str()).collect::<Vec<_>>(),
            ["ДокументСсылка.Поступление", "ДокументСсылка.Списание", "ДокументСсылка.Возврат"]
        );
        assert_eq!(
            fields[1].types[2].description.as_deref(),
            Some("первичный документ.\nПродолжение описания.")
        );
        assert_eq!(fields[2].name, "Сумма");
    }

    /// Parameter structures obey the same field boundaries, including tab alignment.
    #[test]
    fn parameter_fields_do_not_capture_the_next_parameter() {
        let comments = [
            "Parameters:",
            " Data - Structure - options:",
            "\t* Value - String,",
            "\t\t- Number - numeric value",
            "\t* Enabled - Boolean - flag",
            " Other - Date - another parameter",
        ]
        .map(str::to_owned);
        let docs = parse_method_docs(&comments).unwrap();
        assert_eq!(docs.parameters.len(), 2);
        assert_eq!(docs.parameters[0].types.len(), 1);
        let fields = &docs.parameters[0].types[0].parameters;
        assert_eq!(fields.len(), 2);
        assert_eq!(
            fields[0].types.iter().map(|ty| ty.name.as_str()).collect::<Vec<_>>(),
            ["String", "Number"]
        );
        assert_eq!(docs.parameters[1].name, "Other");
    }

    /// Each additional star nests fields under the preceding field's container type.
    #[test]
    fn return_fields_support_recursive_nesting() {
        let comments = [
            "Возвращаемое значение:",
            " Соответствие из КлючИЗначение - объекты и значения:",
            "  * Ключ - ЛюбаяСсылка - ссылка на объект;",
            "  * Значение - Структура:",
            "   ** Ключ - Строка - имя реквизита;",
            "   ** Значение - Произвольный - значение реквизита.",
        ]
        .map(str::to_owned);
        let docs = parse_method_docs(&comments).unwrap();
        let fields = &docs.returned_value[0].parameters;
        assert_eq!(
            fields.iter().map(|field| field.name.as_str()).collect::<Vec<_>>(),
            ["Ключ", "Значение"]
        );
        let nested = &fields[1].types[0].parameters;
        assert_eq!(
            nested.iter().map(|field| field.name.as_str()).collect::<Vec<_>>(),
            ["Ключ", "Значение"]
        );
        assert_eq!(nested[0].types[0].description.as_deref(), Some("имя реквизита;"));
        assert_eq!(nested[1].types[0].name, "Произвольный");
    }
}
