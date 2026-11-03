//! Which documentation lines change the documented parameters when replaced one at a time.
//!
//! A line's effect is decided by the few lines it depends on, so one parse of the block is
//! enough to find them: every line is then judged on that small window instead of the block.

use super::{
    fields, is_collection_type_name, parse_method_docs, parse_parameters,
    parse_parameters_with_roles, section_header, ParameterDoc, Section, TypeDoc,
};
use std::ops::Range;

/// What a line of a `Параметры:` section is to the parser. The level is the field nesting of
/// the line's owner, 0 for the parameter itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum LineRole {
    #[default]
    Blank,
    Parameter,
    FieldMarker(usize),
    Type(usize),
    /// Reaches no name or type, unless the description it adds to can label a type.
    Text {
        level: usize,
        description: DescriptionUse,
    },
}

/// What the description of a line's type means for that type's label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum DescriptionUse {
    /// The type is no collection: its label is its name.
    #[default]
    Prose,
    /// A collection whose description does not open with its element: the label stays the
    /// name, unless a replacement makes the first line open with one.
    CollectionProse,
    /// `Массив из Строка`: the text up to the separator that ends it is the label.
    CollectionLabel,
}

impl DescriptionUse {
    pub(super) fn of(type_doc: Option<&TypeDoc>) -> Self {
        use stdx::case::CaseExt;
        let Some(type_doc) = type_doc.filter(|ty| is_collection_type_name(&ty.name)) else {
            return Self::Prose;
        };
        let opening = type_doc.description.as_deref().unwrap_or_default().trim_start();
        let opening = opening.chars().take(2).collect::<String>().fold_lower();
        if opening == "из" || opening == "of" {
            Self::CollectionLabel
        } else {
            Self::CollectionProse
        }
    }
}

const LABEL_SEPARATORS: [&str; 6] = [" -- ", " — ", " – ", " - ", " -", "- "];

/// A line the lines below it depend on.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    level: usize,
    line: usize,
    is_owner: bool,
}

/// The owners of the current position: a parameter, the field markers above it and, for each,
/// the type that receives what follows. Earlier siblings leave no state behind.
#[derive(Default)]
struct Anchors(Vec<Anchor>);

impl Anchors {
    fn close_levels_from(&mut self, level: usize) {
        self.0.retain(|anchor| anchor.level < level);
    }

    fn update(&mut self, role: LineRole, line: usize) {
        match role {
            LineRole::Blank => {}
            LineRole::Parameter => {
                self.0.clear();
                self.0.push(Anchor { level: 0, line, is_owner: true });
            }
            LineRole::FieldMarker(level) => {
                self.close_levels_from(level);
                self.0.push(Anchor { level, line, is_owner: true });
            }
            LineRole::Type(level) => {
                self.close_levels_from(level + 1);
                self.0.retain(|anchor| anchor.level != level || anchor.is_owner);
                self.0.push(Anchor { level, line, is_owner: false });
            }
            LineRole::Text { level, .. } => self.close_levels_from(level + 1),
        }
    }
}

/// The lines of one description that hold a type's label.
#[derive(Default)]
struct LabelText {
    kept_lines: Vec<usize>,
    separator_counts: [u8; LABEL_SEPARATORS.len()],
}

impl LabelText {
    /// The first line keeps the description a label. The label ends at the separator that the
    /// description holds first, and a replacement can take it away from one line, so of the
    /// other lines only the first two holding each separator can move that end. A description
    /// keeps its lines trimmed, so a dash that opens a line has no space before it there.
    fn add(&mut self, line: usize, text: &str, description: DescriptionUse) {
        let mut keep = self.kept_lines.is_empty();
        if description == DescriptionUse::CollectionLabel {
            let text = text.trim();
            for (separator, count) in LABEL_SEPARATORS.iter().zip(&mut self.separator_counts) {
                if *count < 2 && text.contains(separator) {
                    *count += 1;
                    keep = true;
                }
            }
        }
        if keep {
            self.kept_lines.push(line);
        }
    }
}

/// For each line of a collection's description, the description it belongs to.
fn label_texts(roles: &[LineRole], lines: &[String]) -> (Vec<Option<usize>>, Vec<LabelText>) {
    let mut text_of_line = vec![None; roles.len()];
    let mut texts = Vec::<LabelText>::new();
    let mut open = Vec::<(usize, usize)>::new();
    for (line, role) in roles.iter().enumerate() {
        match *role {
            LineRole::Blank => {}
            LineRole::Parameter => open.clear(),
            LineRole::FieldMarker(level) | LineRole::Type(level) => {
                open.retain(|(open_level, _)| *open_level < level);
            }
            LineRole::Text { level, description } => {
                open.retain(|(open_level, _)| *open_level <= level);
                if description != DescriptionUse::Prose {
                    let text = match open.iter().find(|(open_level, _)| *open_level == level) {
                        Some((_, text)) => *text,
                        None => {
                            texts.push(LabelText::default());
                            open.push((level, texts.len() - 1));
                            texts.len() - 1
                        }
                    };
                    texts[text].add(line, &lines[line], description);
                    text_of_line[line] = Some(text);
                }
            }
        }
    }
    (text_of_line, texts)
}

/// For each line, whether putting its version from `replaced` into `lines`, with every other
/// line left as it is, changes what the documentation says about its parameters: their names,
/// types and structure fields. Prose is not part of that, since no reader takes a meaning from
/// it. Nothing is reported for a block that replacing every line at once leaves the same.
pub fn doc_lines_reshaped_by_replacement(lines: &[String], replaced: &[String]) -> Vec<bool> {
    let mut reshaped = vec![false; lines.len()];
    let parameters = parameters_of(lines);
    if same_parameter_shape(&parameters, &parameters_of(replaced)) {
        return reshaped;
    }

    let block = Block { lines, replaced, parameters };
    let (ways, section) = block.judging_ways();
    for (index, way) in ways.iter().enumerate() {
        if *way == JudgingWay::WholeBlock {
            reshaped[index] = block.reshaped_by_whole_block(index);
        }
    }
    if let Some(section) = section {
        block.judge_section_lines(section, &ways, &mut reshaped);
    }
    reshaped
}

fn parameters_of(lines: &[String]) -> Vec<ParameterDoc> {
    parse_method_docs(lines).map(|docs| docs.parameters).unwrap_or_default()
}

/// How a line's replacement is judged. Only a replacement that moves a boundary of a section or
/// a field block, or decides whether the block is a link, needs the whole block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JudgingWay {
    Window,
    /// The line becomes the header of a section other than the parameters.
    ClosesSection,
    WholeBlock,
}

/// What the lines from some point of a section to its end add to the parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SectionTail {
    Nothing,
    MaybeSomething,
    Entries,
}

impl SectionTail {
    fn of_role(role: LineRole) -> Self {
        match role {
            LineRole::Parameter | LineRole::Type(0) => Self::Entries,
            LineRole::Blank
            | LineRole::Text {
                description: DescriptionUse::Prose | DescriptionUse::CollectionProse,
                ..
            } => Self::Nothing,
            _ => Self::MaybeSomething,
        }
    }
}

struct Block<'a> {
    lines: &'a [String],
    replaced: &'a [String],
    parameters: Vec<ParameterDoc>,
}

impl Block<'_> {
    /// Also returns the section the parameters come from: only a replacement that moves a
    /// boundary reaches them from outside it.
    fn judging_ways(&self) -> (Vec<JudgingWay>, Option<Range<usize>>) {
        let is_filled = |line: &String| !line.trim().is_empty();
        let first_filled = self.lines.iter().position(is_filled);
        let last_filled = self.lines.iter().rposition(is_filled);

        let mut ways = Vec::with_capacity(self.lines.len());
        let mut section = None;
        let mut in_parameters = false;
        for (index, (line, new_line)) in self.lines.iter().zip(self.replaced).enumerate() {
            let kind = section_header(line.trim(), in_parameters).map(|(kind, _)| kind);
            let new_kind = section_header(new_line.trim(), in_parameters).map(|(kind, _)| kind);
            let decides_link = Some(index) == first_filled || Some(index) == last_filled;
            ways.push(if line == new_line {
                JudgingWay::Window
            } else if decides_link || moves_field_boundary(line, new_line) {
                JudgingWay::WholeBlock
            } else if kind.is_none() && new_kind.is_some_and(|new| new != Section::Parameters) {
                JudgingWay::ClosesSection
            } else if kind != new_kind {
                JudgingWay::WholeBlock
            } else {
                JudgingWay::Window
            });
            if let Some(kind) = kind {
                in_parameters = kind == Section::Parameters;
                section = match (kind, section) {
                    (Section::Parameters, _) => Some(index + 1..self.lines.len()),
                    (_, Some(open)) if open.end == self.lines.len() => Some(open.start..index),
                    (_, closed) => closed,
                };
            }
        }
        (ways, section)
    }

    fn reshaped_by_whole_block(&self, index: usize) -> bool {
        let mut candidate = self.lines.to_vec();
        candidate[index].clone_from(&self.replaced[index]);
        !same_parameter_shape(&self.parameters, &parameters_of(&candidate))
    }

    fn judge_section_lines(
        &self,
        section: Range<usize>,
        ways: &[JudgingWay],
        reshaped: &mut [bool],
    ) {
        let section_lines = &self.lines[section.clone()];
        let mut roles = vec![LineRole::default(); section_lines.len()];
        parse_parameters_with_roles(section_lines, Some(&mut roles));

        let mut tails = vec![SectionTail::Nothing; section_lines.len() + 1];
        for (offset, role) in roles.iter().enumerate().rev() {
            tails[offset] = tails[offset + 1].max(SectionTail::of_role(*role));
        }

        let (text_of_line, label_texts) = label_texts(&roles, section_lines);
        let mut anchors = Anchors::default();
        let mut previous = None;
        for (offset, line) in section_lines.iter().enumerate() {
            let index = section.start + offset;
            let new_line = &self.replaced[index];
            match ways[index] {
                JudgingWay::WholeBlock => {}
                JudgingWay::ClosesSection => {
                    // The parameters end above this line, and an entry below it is gone.
                    reshaped[index] = match tails[offset] {
                        SectionTail::Nothing => false,
                        SectionTail::MaybeSomething => self.reshaped_by_whole_block(index),
                        SectionTail::Entries => true,
                    };
                }
                JudgingWay::Window if line != new_line => {
                    let label_lines = text_of_line[offset]
                        .iter()
                        .flat_map(|text| label_texts[*text].kept_lines.iter().copied());
                    let mut window: Vec<usize> = anchors
                        .0
                        .iter()
                        .map(|anchor| anchor.line)
                        .chain(label_lines)
                        .chain(previous)
                        .chain([offset])
                        .collect();
                    window.sort_unstable();
                    window.dedup();
                    reshaped[index] = window_is_reshaped(&window, offset, section_lines, new_line);
                }
                JudgingWay::Window => {}
            }
            let role = roles[offset];
            anchors.update(role, offset);
            if role != LineRole::Blank {
                previous = Some(offset);
            }
        }
    }
}

/// Whether a field block would end elsewhere: it ends by a line's marker and indentation, and
/// a blank line does not end it. Only a soft hyphen before the text changes these.
fn moves_field_boundary(line: &str, new_line: &str) -> bool {
    line.trim().is_empty() != new_line.trim().is_empty()
        || fields::indentation(line) != fields::indentation(new_line)
        || fields::marker_depth(line.trim()) != fields::marker_depth(new_line.trim())
}

fn window_is_reshaped(window: &[usize], offset: usize, lines: &[String], new_line: &str) -> bool {
    let before: Vec<String> = window.iter().map(|&line| lines[line].clone()).collect();
    let after: Vec<String> = window
        .iter()
        .map(|&line| if line == offset { new_line.to_string() } else { lines[line].clone() })
        .collect();
    !same_parameter_shape(&parse_parameters(&before), &parse_parameters(&after))
}

fn same_parameter_shape(left: &[ParameterDoc], right: &[ParameterDoc]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.name == right.name
                && left.types.len() == right.types.len()
                && left.types.iter().zip(&right.types).all(|(left, right)| {
                    left.is_hyperlink == right.is_hyperlink
                        && left.type_label_and_description().0
                            == right.type_label_and_description().0
                        && same_parameter_shape(&left.parameters, &right.parameters)
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pairs of a line and its replacement, chosen so that every role can change into another.
    const VOCABULARY: &[(&str, &str)] = &[
        ("Параметры:", "Параметры:"),
        ("Parameters:", "Parameters:"),
        ("\u{AD}Параметры:", "Параметры:"),
        ("Параметры\u{A0}:", "Параметры :"),
        ("Возвращаемое значение:", "Возвращаемое значение:"),
        ("Возвращаемое\u{A0}значение:", "Возвращаемое значение:"),
        ("Устар\u{AD}ела.", "Устарела."),
        ("Пример:", "Пример:"),
        ("Описание.", "Описание."),
        ("", ""),
        ("\u{AD}", ""),
        ("\u{A0}", " "),
        ("см. Модуль.Метод", "см. Модуль.Метод"),
        ("с\u{AD}м. Модуль.Метод", "см. Модуль.Метод"),
        (" Имя - Строка", " Имя - Строка"),
        (" Имя\u{A0}- Строка", " Имя - Строка"),
        (" Параметр - Устар\u{AD}ела", " Параметр - Устарела"),
        (" Имя \u{2013} Строка", " Имя - Строка"),
        (" Имя\u{A0}-\u{A0}Строка", " Имя - Строка"),
        (" Имя - Строка \u{2014} описание", " Имя - Строка - описание"),
        (" Имя - Строка - две\u{A0}части", " Имя - Строка - две части"),
        (" И\u{AD}мя - Строка", " Имя - Строка"),
        (" Второй - Строка, Число", " Второй - Строка, Число"),
        (" Результат - Строка", " Результат - Строка"),
        (" Результат \u{2014} Строка", " Результат - Строка"),
        (" Пустой - ,", " Пустой - ,"),
        (" - Число", " - Число"),
        (" -\u{A0}Число", " - Число"),
        (" - Число \u{2013} описание", " - Число - описание"),
        (" \u{2013} Число", " - Число"),
        (" Спр.Х - описание", " Спр.Х - описание"),
        (" Спр.Х \u{2013} описание", " Спр.Х - описание"),
        (" Спр.Х\u{A0}-\u{A0}описание", " Спр.Х - описание"),
        (" две\u{A0}части", " две части"),
        (" просто текст", " просто текст"),
        (" текст \u{2014} с тире", " текст - с тире"),
        (" текст -- с тире", " текст -- с тире"),
        (" Коллекция - Массив - из Строка", " Коллекция - Массив - из Строка"),
        (" Коллекция - Массив", " Коллекция - Массив"),
        (" из\u{A0}Строка", " из Строка"),
        (" \u{AD}из Строка", " из Строка"),
        (" из Числа \u{2014} текст", " из Числа - текст"),
        (" of Числа", " of Числа"),
        (" Набор - Структура:", " Набор - Структура:"),
        ("   * Поле - Строка", "   * Поле - Строка"),
        ("   * Поле\u{A0}-\u{A0}Строка", "   * Поле - Строка"),
        ("   * Поле \u{2013} Строка", "   * Поле - Строка"),
        ("   * Поле - Строка,", "   * Поле - Строка,"),
        ("   * Поле - Строка \u{2014},", "   * Поле - Строка -,"),
        ("   * Пусто - ,", "   * Пусто - ,"),
        ("       Спр.Х\u{A0}-\u{A0}Строка,", "       Спр.Х - Строка,"),
        ("       - Число", "       - Число"),
        ("       -\u{A0}Число", "       - Число"),
        ("       две\u{A0}части", "       две части"),
        ("       из\u{A0}Строка", "       из Строка"),
        ("     ** Вложенное\u{A0}- Строка", "     ** Вложенное - Строка"),
        ("     ** Вложенное - Массив - из Строка", "     ** Вложенное - Массив - из Строка"),
        (" \u{AD}* Поле - Строка", " * Поле - Строка"),
        ("\t* Поле\t-\tСтрока", "\t* Поле\t-\tСтрока"),
        ("\t\t- Дата", "\t\t- Дата"),
        ("\u{A0}\u{A0}* Поле - Строка", "  * Поле - Строка"),
    ];

    /// The definition the fast path must keep to: every line re-parses the whole block.
    fn reshaped_by_full_parse(lines: &[String], replaced: &[String]) -> Vec<bool> {
        let parameters = parameters_of(lines);
        if same_parameter_shape(&parameters, &parameters_of(replaced)) {
            return vec![false; lines.len()];
        }
        (0..lines.len())
            .map(|index| {
                let mut candidate = lines.to_vec();
                candidate[index].clone_from(&replaced[index]);
                !same_parameter_shape(&parameters, &parameters_of(&candidate))
            })
            .collect()
    }

    fn block_of(pairs: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
        let lines = pairs.iter().map(|(line, _)| line.to_string()).collect();
        let replaced = pairs.iter().map(|(_, new_line)| new_line.to_string()).collect();
        (lines, replaced)
    }

    #[test]
    fn windows_agree_with_parsing_the_whole_block_on_random_blocks() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |limit: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % limit as u64) as usize
        };
        let mut reshaped_lines = 0;
        for _ in 0..8_000 {
            let length = 1 + next(14);
            let mut pairs: Vec<(&str, &str)> =
                (0..length).map(|_| VOCABULARY[next(VOCABULARY.len())]).collect();
            if next(4) != 0 {
                pairs.insert(0, VOCABULARY[0]);
            }
            let (lines, replaced) = block_of(&pairs);
            let expected = reshaped_by_full_parse(&lines, &replaced);
            reshaped_lines += expected.iter().filter(|reshaped| **reshaped).count();
            assert_eq!(
                doc_lines_reshaped_by_replacement(&lines, &replaced),
                expected,
                "block: {lines:#?}"
            );
        }
        assert!(reshaped_lines > 5_000, "only {reshaped_lines} reshaped lines were exercised");
    }

    /// Asserts the full parse's verdict on `line` too, so a case cannot go vacuous.
    fn assert_verdict_on_line(pairs: &[(&str, &str)], line: usize, verdict: bool) {
        let (lines, replaced) = block_of(pairs);
        let expected = reshaped_by_full_parse(&lines, &replaced);
        assert_eq!(expected[line], verdict, "full parse of {lines:#?}");
        assert_eq!(doc_lines_reshaped_by_replacement(&lines, &replaced), expected);
    }

    #[test]
    fn a_line_that_becomes_blank_keeps_the_field_block_open_below_it() {
        assert_verdict_on_line(
            &[
                ("Параметры:", "Параметры:"),
                (" Имя - Структура", " Имя - Структура"),
                ("   * Поле - Структура", "   * Поле - Структура"),
                ("\u{AD}", ""),
                ("     ** Плохое\u{A0}- Строка", "     ** Плохое - Строка"),
                ("     ** Вложенное - Строка", "     ** Вложенное - Строка"),
            ],
            3,
            true,
        );
    }

    #[test]
    fn a_line_that_gains_indentation_stays_in_the_field_block() {
        assert_verdict_on_line(
            &[
                ("Параметры:", "Параметры:"),
                (" Имя - Структура", " Имя - Структура"),
                ("   * Поле - Строка", "   * Поле - Строка"),
                ("   \u{AD}   текст", "      текст"),
                ("       ещё текст", "       ещё текст"),
                ("       - Число", "       - Число"),
            ],
            3,
            true,
        );
    }

    #[test]
    fn a_line_that_becomes_a_marker_takes_the_lines_below_it() {
        assert_verdict_on_line(
            &[
                ("Параметры:", "Параметры:"),
                (" Имя - Структура", " Имя - Структура"),
                ("   \u{AD}* Плохое", "   * Плохое"),
                ("       ещё текст", "       ещё текст"),
                ("       - Число", "       - Число"),
            ],
            2,
            true,
        );
    }

    #[test]
    fn prose_above_a_line_closes_the_union_of_its_field() {
        assert_verdict_on_line(
            &[
                ("Параметры:", "Параметры:"),
                (" Набор - Структура:", " Набор - Структура:"),
                ("   * Поле - Строка,", "   * Поле - Строка,"),
                ("       две\u{A0}части", "       две части"),
                ("       Спр.Х\u{A0}-\u{A0}Строка,", "       Спр.Х - Строка,"),
                (" Имя\u{A0}- Строка", " Имя - Строка"),
            ],
            4,
            false,
        );
    }

    #[test]
    fn a_separator_at_the_start_of_a_line_does_not_count_for_the_label() {
        assert_verdict_on_line(
            &[
                ("Параметры:", "Параметры:"),
                (" Коллекция - Массив - из Строка", " Коллекция - Массив - из Строка"),
                (" из Числа \u{2014} текст", " из Числа - текст"),
                (" \u{2013} Число", " \u{2013} Число"),
                (" \u{2013} Число", " \u{2013} Число"),
                (" текст \u{2013} с тире", " текст \u{2013} с тире"),
            ],
            2,
            true,
        );
    }

    #[test]
    fn a_line_is_judged_with_the_others_left_as_they_are() {
        let (lines, replaced) = block_of(&[
            ("Параметры:", "Параметры:"),
            (" Первый\u{A0}- Строка", " Первый - Строка"),
            (" две\u{A0}части", " две части"),
            (" Коллекция - Массив - из Строка", " Коллекция - Массив - из Строка"),
            (" две\u{A0}части", " две части"),
            (" хвост \u{2014} текст", " хвост - текст"),
            (" за\u{A0}разделителем", " за разделителем"),
        ]);
        assert_eq!(
            doc_lines_reshaped_by_replacement(&lines, &replaced),
            [false, true, false, false, true, true, false]
        );
    }
}
