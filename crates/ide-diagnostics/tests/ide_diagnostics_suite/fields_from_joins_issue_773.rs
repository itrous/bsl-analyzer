//! The module of issue #773 through the production path, with and without the types of the
//! fields it queries.
//!
//! The types come from a synthetic configuration on disk, so no installed platform is needed.
//! Typed, the module is silent in every procedure; untyped, the replacements of `ЕСТЬNULL` are
//! not known to survive the conversion and the three uses they would hide are reported. Each
//! procedure also has its control: the same procedure with the part that hides the field taken
//! away reports the field again.

use std::path::Path;

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use vfs::{FileId, FileSet, VfsPath};

const CODE: DiagnosticCode = DiagnosticCode::FieldsFromJoinsWithoutIsNull;

const MODULE: &str = include_str!("../../src/handlers/fixtures/fields_from_joins_issue_773.bsl");

const HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.20">"#;

const NUMBER_15_4: &str = "<v8:Type>xs:decimal</v8:Type><v8:NumberQualifiers><v8:Digits>15</v8:Digits><v8:FractionDigits>4</v8:FractionDigits><v8:AllowedSign>Any</v8:AllowedSign></v8:NumberQualifiers>";
const NUMBER_15_2: &str = "<v8:Type>xs:decimal</v8:Type><v8:NumberQualifiers><v8:Digits>15</v8:Digits><v8:FractionDigits>2</v8:FractionDigits><v8:AllowedSign>Any</v8:AllowedSign></v8:NumberQualifiers>";
const NUMBER_10_0: &str = "<v8:Type>xs:decimal</v8:Type><v8:NumberQualifiers><v8:Digits>10</v8:Digits><v8:FractionDigits>0</v8:FractionDigits><v8:AllowedSign>Any</v8:AllowedSign></v8:NumberQualifiers>";
const DATE_TIME: &str = "<v8:Type>xs:dateTime</v8:Type><v8:DateQualifiers><v8:DateFractions>DateTime</v8:DateFractions></v8:DateQualifiers>";
const STRING_10: &str = "<v8:Type>xs:string</v8:Type><v8:StringQualifiers><v8:Length>10</v8:Length><v8:AllowedLength>Variable</v8:AllowedLength></v8:StringQualifiers>";

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture file has a parent"))
        .expect("create fixture directory");
    std::fs::write(path, text).expect("write fixture file");
}

fn member(kind: &str, uuid: u32, name: &str, attr_type: &str) -> String {
    format!(
        "<{kind} uuid=\"7730{uuid:04}-0000-0000-0000-000000000000\"><Properties><Name>{name}</Name><Type>{attr_type}</Type></Properties></{kind}>"
    )
}

fn register(uuid: u32, name: &str, members: &[String]) -> String {
    format!(
        r#"{HEADER}
    <InformationRegister uuid="7731{uuid:04}-0000-0000-0000-000000000000">
        <Properties>
            <Name>{name}</Name>
            <InformationRegisterPeriodicity>Day</InformationRegisterPeriodicity>
            <WriteMode>Independent</WriteMode>
        </Properties>
        <ChildObjects>{}</ChildObjects>
    </InformationRegister>
</MetaDataObject>"#,
        members.concat()
    )
}

/// The objects the module queries, with the fields the queries use. `course` is the type of
/// the rate of the currency, the one field whose type a control changes.
fn write_configuration(root: &Path, course: &str) {
    write(
        root,
        "Configuration.xml",
        &format!(
            r#"{HEADER}
    <Configuration uuid="77300000-0000-0000-0000-000000000001">
        <Properties><Name>Синтетика</Name></Properties>
        <ChildObjects>
            <CommonModule>Тест</CommonModule>
            <Document>Заказ</Document>
            <InformationRegister>Цены</InformationRegister>
            <InformationRegister>КурсыВалют</InformationRegister>
            <InformationRegister>ПараметрыОС</InformationRegister>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "CommonModules/Тест.xml",
        &format!(
            r#"{HEADER}
    <CommonModule uuid="77300000-0000-0000-0000-000000000002">
        <Properties><Name>Тест</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#
        ),
    );
    let order_attributes = [
        member("Attribute", 1, "Номенклатура", "<v8:Type>cfg:CatalogRef.Номенклатура</v8:Type>"),
        member("Attribute", 2, "Валюта", "<v8:Type>cfg:CatalogRef.Валюты</v8:Type>"),
        member(
            "Attribute",
            3,
            "ОсновноеСредство",
            "<v8:Type>cfg:CatalogRef.ОсновныеСредства</v8:Type>",
        ),
        member("Attribute", 4, "Сумма", NUMBER_15_2),
    ]
    .concat();
    let items = format!(
        "<TabularSection uuid=\"77300000-0000-0000-0000-000000000003\"><Properties><Name>Товары</Name></Properties><ChildObjects>{}</ChildObjects></TabularSection>",
        member("Attribute", 5, "Номенклатура", "<v8:Type>cfg:CatalogRef.Номенклатура</v8:Type>"),
    );
    write(
        root,
        "Documents/Заказ.xml",
        &format!(
            r#"{HEADER}
    <Document uuid="77300000-0000-0000-0000-000000000004">
        <Properties><Name>Заказ</Name></Properties>
        <ChildObjects>{order_attributes}{items}</ChildObjects>
    </Document>
</MetaDataObject>"#
        ),
    );
    write(
        root,
        "InformationRegisters/Цены.xml",
        &register(
            1,
            "Цены",
            &[
                member(
                    "Dimension",
                    10,
                    "Номенклатура",
                    "<v8:Type>cfg:CatalogRef.Номенклатура</v8:Type>",
                ),
                member("Dimension", 11, "ТипЦен", "<v8:Type>cfg:CatalogRef.ТипыЦен</v8:Type>"),
                member("Resource", 12, "Цена", NUMBER_15_2),
            ],
        ),
    );
    write(
        root,
        "InformationRegisters/КурсыВалют.xml",
        &register(
            2,
            "КурсыВалют",
            &[
                member("Dimension", 20, "Валюта", "<v8:Type>cfg:CatalogRef.Валюты</v8:Type>"),
                member("Resource", 21, "Курс", course),
                member("Resource", 22, "Кратность", NUMBER_10_0),
            ],
        ),
    );
    write(
        root,
        "InformationRegisters/ПараметрыОС.xml",
        &register(
            3,
            "ПараметрыОС",
            &[
                member(
                    "Dimension",
                    30,
                    "ОсновноеСредство",
                    "<v8:Type>cfg:CatalogRef.ОсновныеСредства</v8:Type>",
                ),
                member("Resource", 31, "ДатаНачала", DATE_TIME),
                member("Resource", 32, "ДатаСписания", DATE_TIME),
                member("Resource", 33, "ЕжемесячныйИзнос", NUMBER_15_2),
            ],
        ),
    );
}

/// Where the module sits: with the synthetic configuration, or in a project with none.
#[derive(Clone, Copy)]
enum Metadata {
    Absent,
    Typed { course: &'static str },
}

/// The uses the rule reports in `source`: the line (from 1) and the text under each.
fn reported(source: &str, metadata: Metadata) -> Vec<(usize, String)> {
    let dir = tempfile::tempdir().expect("create fixture directory");
    let root = dir.path().join("cf");
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    match metadata {
        Metadata::Absent => files.insert(file_id, VfsPath::new("/test.bsl")),
        Metadata::Typed { course } => {
            write_configuration(&root, course);
            let path = root.join("CommonModules/Тест/Ext/Module.bsl");
            write(&root, "CommonModules/Тест/Ext/Module.bsl", source);
            files.insert(file_id, VfsPath::new(path.to_string_lossy().into_owned()));
            db.set_all_config_paths(
                vec![(None, root.clone())],
                Some(bsl_metadata::ExtensionScope::InfoBase),
            );
        }
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    db.set_workspace_load_complete(true);

    let mut found: Vec<_> =
        ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::default())
            .into_iter()
            .filter(|d| d.code == CODE)
            .map(|d| {
                let start = usize::from(d.range.start());
                (source[..start].matches('\n').count() + 1, source[d.range].to_string())
            })
            .collect();
    found.sort();
    found
}

/// `source` with the one occurrence of `from` replaced; the occurrence must exist, or the
/// control would test nothing.
fn changed(source: &str, from: &str, to: &str) -> String {
    assert_eq!(source.matches(from).count(), 1, "`{from}` must occur once in the module");
    source.replacen(from, to, 1)
}

const TYPED: Metadata = Metadata::Typed { course: NUMBER_15_4 };

fn line(number: usize, text: &str) -> (usize, String) {
    (number, text.to_string())
}

#[test]
fn the_module_is_silent_when_the_types_of_the_queried_fields_are_known() {
    assert_eq!(reported(MODULE, TYPED), Vec::new());
}

#[test]
fn without_metadata_the_filtered_and_owner_uses_are_silent_and_the_replacements_are_not_trusted() {
    // Lines 7 and 22 are hidden by `ГДЕ`, 57 by the comparison of a date difference, 78 by the
    // owner of the tabular section; 41 (both fields) and 62 stay because the replacements
    // of `ЕСТЬNULL` are not known to survive the conversion to the unknown type.
    assert_eq!(
        reported(MODULE, Metadata::Absent),
        [line(41, "Курсы.Кратность"), line(41, "Курсы.Курс"), line(62, "ОС.ДатаСписания")]
    );
}

#[test]
fn the_type_of_the_rate_is_what_silences_the_branch_after_isnull() {
    // The control of the typed run: the rate as a string cannot take the number 0 as its
    // replacement, so the branch is reported again even though the metadata is present.
    assert_eq!(
        reported(MODULE, Metadata::Typed { course: STRING_10 }),
        [line(41, "Курсы.Кратность"), line(41, "Курсы.Курс")]
    );
}

#[test]
fn removing_the_filter_reports_the_fields_it_hid() {
    // The first procedure: `ГДЕ` with the period of the price removed.
    let first = changed(
        MODULE,
        "Цены.Номенклатура\n\t|ГДЕ\n\t|\tЦены.Период МЕЖДУ &Начало И &Конец\";",
        "Цены.Номенклатура\";",
    );
    // The second procedure: `ГДЕ` with the posted flag and the price type removed.
    let second = changed(
        MODULE,
        "Цены.Номенклатура\n\t|ГДЕ\n\t|\tДок.Проведен\n\t|\tИ Цены.ТипЦен В (&ТипыЦен)\";",
        "Цены.Номенклатура\";",
    );
    for metadata in [TYPED, Metadata::Absent] {
        for (source, procedure) in [(&first, "ФильтрВГде"), (&second, "ФильтрВГдеРавно")]
        {
            let prices: Vec<_> = reported(source, metadata)
                .into_iter()
                .filter(|(_, text)| text == "Цены.Цена")
                .collect();
            assert_eq!(prices.len(), 1, "{procedure}: {prices:?}");
        }
    }
    // The control: the unchanged module reports neither price.
    assert!(!reported(MODULE, TYPED).iter().any(|(_, text)| text == "Цены.Цена"));
}

#[test]
fn a_flag_in_place_of_the_condition_reports_the_branches_it_hid() {
    // After `ЕСТЬNULL(Курсы.Курс, 0) = 0` the `ИНАЧЕ` is safe; after a parameter it is not.
    let rate = changed(MODULE, "КОГДА ЕСТЬNULL(Курсы.Курс, 0) = 0", "КОГДА &Флаг");
    for metadata in [TYPED, Metadata::Absent] {
        assert_eq!(
            reported(&rate, metadata)
                .into_iter()
                .filter(|(number, _)| *number == 41)
                .map(|(_, text)| text)
                .collect::<Vec<_>>(),
            ["Курсы.Кратность", "Курсы.Курс"]
        );
    }

    // After a parameter the date difference no longer hides the monthly wear, nor the
    // comparison with the minimal date the date of write-off.
    let wear =
        changed(MODULE, "КОГДА РАЗНОСТЬДАТ(ОС.ДатаНачала, &Дата, МЕСЯЦ) >= 0", "КОГДА &Флаг");
    let written_off = changed(
        MODULE,
        "КОГДА ЕСТЬNULL(ОС.ДатаСписания, ДАТАВРЕМЯ(1, 1, 1)) <> ДАТАВРЕМЯ(1, 1, 1)",
        "КОГДА &Флаг",
    );
    for metadata in [TYPED, Metadata::Absent] {
        let wear_found: Vec<_> = reported(&wear, metadata)
            .into_iter()
            .filter(|(_, text)| text == "ОС.ЕжемесячныйИзнос")
            .collect();
        assert_eq!(wear_found, [line(57, "ОС.ЕжемесячныйИзнос")]);
        let off_found: Vec<_> = reported(&written_off, metadata)
            .into_iter()
            .filter(|(_, text)| text == "ОС.ДатаСписания")
            .collect();
        assert_eq!(off_found, [line(62, "ОС.ДатаСписания")]);
    }
}

#[test]
fn a_weakened_owner_condition_reports_the_owner_again() {
    let weakened = changed(
        MODULE,
        "ПО Товары.Ссылка = Док.Ссылка\n",
        "ПО Товары.Ссылка = Док.Ссылка И Док.Проведен\n",
    );
    for metadata in [TYPED, Metadata::Absent] {
        let found = reported(&weakened, metadata);
        assert!(found.contains(&line(78, "Док.Дата")), "{found:?}");
    }
    // The control: the unchanged condition leaves the owner silent in the same run.
    assert!(!reported(MODULE, TYPED).iter().any(|(number, _)| *number == 78));
}
