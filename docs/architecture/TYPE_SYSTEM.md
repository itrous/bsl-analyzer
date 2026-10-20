# Архитектура системы типов BSL Analyzer

Документ фиксирует текущую модель разрешения типов данных в проекте.
Референс — `rust-analyzer`, адаптированный под особенности BSL.

## Принципы

- **Одна точка разрешения** для выражения, имени и пути.
- Источники знаний о типах (`bsl-platform`, `bsl-metadata`, JSDoc,
  ручные аннотации менеджеров) подключаются через явные адаптеры, а
  не через глобальные singleton'ы в hot-path.
- IDE-слои (`ide`, `ide-completion`, `ide-diagnostics`, `ide-assists`)
  не обращаются к `bsl-platform` / `bsl-metadata` / `hir-ty` напрямую.
  Публичная точка — `hir::Semantics` и `hir::Type`.
- Инвалидация — через Salsa inputs: изменение конфигурации или
  исходника корректно перестраивает затронутые типы.

## Слои

```
Frameworks & Drivers   : lsp-server, salsa, tokio, lsp-types
Interface Adapters     : ide, ide-completion, ide-diagnostics, ide-assists
Application / Facade   : hir  (Semantics, Type, Method, Field, Module)
Domain Services        : hir-ty (lowering, inference), hir-def (resolver, bodies),
                         bsl-platform, bsl-metadata   — адаптеры знаний
Entities               : Ty, TypeRef, MetadataKind, FunctionSignature, Name
```

Правила зависимостей:

- `Entities` не зависят от `db` / `salsa` / `resolver`, только от std
  и `smol_str`.
- `hir-def` зависит от `syntax` и entities.
- `hir-ty` зависит от `hir-def`, entities и от адаптеров знаний
  (`bsl-platform`, `bsl-metadata`) через trait-интерфейсы, без
  обращения к глобальным singleton'ам.
- `hir` — единственный крейт, зависящий одновременно от `hir-ty` и
  `hir-def`. Он собирает OO-фасад `hir::Type`.
- `ide*` зависят только от `hir`.
- Запрещено: `hir-def → hir-ty`, `hir-ty → hir`, `ide* → bsl-platform`
  напрямую.

## Стадии жизненного цикла типа

```
Source          : BSL / XML / JSDoc
 ↓ parse
AST             : syntax::ast
 ↓ item_tree + body lowering
HIR             : hir-def (Body, SymbolTree, ItemTree)
 ↓ type ref extraction
TypeRef         : синтаксическое представление типа (из JSDoc, XML, `Новый`)
 ↓ TyLoweringContext
Ty              : семантический тип (резолв имён, MDO, платформа)
 ↓ InferenceContext (+ narrowing overlay)
InferenceResult : тип каждого выражения + диагностики
 ↓ hir::Type facade
IDE features    : completion, hover, diagnostics, signature help
```

### `TypeRef`

Синтаксическое представление типа — сущность в `hir-def`. Источники:

- описания типов из XML-метаданных (`Справочник.Номенклатура`,
  `СправочникСсылка.Номенклатура`, `ОпределяемыйТип.Х`);
- JSDoc-комментарии к процедурам и функциям (`// Параметры:`,
  `// Возвращаемое значение:`);
- операторы `Новый Тип(...)`.

`TypeRef` — arena-индексируемая сущность без привязки к db.

### `Ty`

Семантический тип. Варианты:

- примитивы: `Number`, `String`, `Boolean`, `Date`, `Undefined`,
  `Null`;
- `Ty::Array`, `Ty::Structure`, `Ty::Map`, `Ty::ValueTable`,
  `Ty::ValueList`, `Ty::Type`;
- `Ty::PlatformObject(Name)` — платформенные объекты типа `Запрос`;
- `Ty::ManagerCollection(MdoType)` — глобалы `Документы`,
  `Справочники`, `РегистрыСведений`;
- `Ty::ObjectManager { kind, name }` — конкретный менеджер
  (`Документы.ПКО`);
- `Ty::MetadataRef { kind: MetadataKind, name }` — ссылка / объект
  MDO. `MetadataKind` покрывает Catalog / Document / Enum / Task /
  BusinessProcess / ExchangePlan / ChartOfAccounts (+ Ref / Object),
  InformationRegister / AccumulationRegister / AccountingRegister /
  CalculationRegister (Ref, RecordManager / RecordSet для
  доступных), TabularSection / TabularSectionRow с `parent: MdoType`,
  RegisterDimension / RegisterResource / RegisterAttribute с
  `parent` для регистров;
- `Ty::ThisObject { owner: (MdoType, Name) }` — самоссылка в
  контексте модуля объекта / менеджера;
- `Ty::Union(Arc<[Ty]>)` — объединения из `ОписаниеТипов`,
  JSDoc-списков через запятую, XML `Composite`. Smart-constructor
  делает flatten + sort + dedup + collapse-to-singleton;
- `Ty::Function { params, ret }` — первоклассные процедуры / функции;
- `Ty::Unknown` — провал инференса.

`Ty` не знает про `db` и живёт на уровне entities.

### `TyLoweringContext`

Чистый адаптер `TypeRef → Ty`. Использует `Resolver` для разрешения
имён и адаптеры знаний для доступа к `Configuration` и списку
платформенных типов. Вся логика lowering'а — `Expr::New`, JSDoc,
`Тип("…")`, XML — проходит через этот контекст.

### Единая точка `infer_expr`

Inference использует тот же `Resolver`, что и Semantics:

- `Expr::Path(name)` → `Resolver::resolve_name` → `def_to_ty`;
- `Expr::Field { base, field }` → `field_lookup::lookup_field`
  (type-directed по receiver'у: ManagerCollection → MDO-объект,
  ObjectManager → manager prop, MetadataRef → реквизит через
  `Configuration`, PlatformObject / collection / primitive →
  `platform_property_lookup::lookup_platform_property` — свойства из
  `platform_data.json` с `is_readonly` флагом и union return types);
- `Expr::Call { callee, args }` — три ветки:
  - quailfied `CommonModule.Method()` → `resolve_qualified_call`;
  - 3-level `Документы.ПКО.Метод()` → `infer_three_level_call`;
  - fluent `receiver.method()` (lowering `Expr::Call { callee:
    Expr::Field }`) → `method_lookup::lookup_method`;
  - обычный callee → `Ty::Function` вариант.

### `hir::Type` (OO-фасад)

Единственный публичный тип-API для IDE:

```rust
pub struct Type<'db, DB> { db: &'db DB, source_root: SourceRootId, inner: hir_ty::Ty }

impl<'db, DB: HirDatabase> Type<'db, DB> {
    pub fn display_name(&self) -> String;
    pub fn methods(&self) -> Vec<Method<'db, DB>>;
    pub fn fields(&self)  -> Vec<Field<'db, DB>>;
    pub fn method_return_type(&self, name: &Name) -> Option<Self>;
    pub fn field_type(&self, name: &Name) -> Option<Self>;
    pub fn is_ref_type(&self) -> bool;
    pub fn manager(&self) -> Option<Self>;
    pub fn is_assignable_to(&self, other: &Self) -> bool;
}
```

Все IDE-фичи обращаются только к нему. `platform_completion` и
`mdo_completion` не знают про `bsl_platform::PlatformData::instance()`
на type-path.

## Salsa-модель

```rust
#[salsa::db]
pub trait HirDatabase: DefDatabase {
    fn infer(&self, file_id: FileId) -> Arc<InferenceResult>;
    fn type_of_expr(&self, file_id: FileId, owner: DefWithBodyId, expr: ExprId) -> Ty;
    fn narrow_query(&self, file_id: FileId, owner: DefWithBodyId) -> Arc<NarrowResult>;

    fn ty_of_method_signature(&self, method: MethodId) -> Arc<FunctionSignature>;
    fn ty_of_mdo_attribute(&self, mdo: MdoRef, attr: Name) -> Ty;
    fn ty_of_this_object(&self, file_id: FileId) -> Ty;

    fn configurations(&self, src: SourceRootId) -> Vec<VisibleConfig>;
    fn manager_methods(&self, kind: MdoType) -> Arc<ManagerMethodTable>;
}
```

`infer` транзитивно зависит от `module_bodies` и `configurations`;
изменение XML или BSL-исходника автоматически сбрасывает кэш вниз по
графу. `narrow_query` отдельный — overlay не пересчитывает базовый
`infer`.

## Таблица резолва выражений

| Выражение BSL | `Ty` |
|---|---|
| литералы | Number / String / Date / Boolean / Undefined / Null |
| `Новый Массив` | `Ty::Array` |
| `Новый Запрос` | `Ty::PlatformObject("Запрос")` |
| `Запрос.Выполнить()` | `Ty::PlatformObject("РезультатЗапроса")` |
| `Документы` | `Ty::ManagerCollection(Document)` |
| `Документы.ПКО` | `Ty::ObjectManager { Document, "ПКО" }` |
| `Документы.ПКО.СоздатьДокумент()` | `Ty::MetadataRef { DocumentObject, "ПКО" }` |
| `Документы.ПКО.ПустаяСсылка()` | `Ty::MetadataRef { DocumentRef, "ПКО" }` |
| `Документ.Сумма` | тип реквизита из `Configuration` |
| `Документ.Товары` | `Ty::MetadataRef { TabularSection, "ПКО.Товары" }` |
| `Документ.Товары.Добавить()` | `Ty::MetadataRef { TabularSectionRow, "ПКО.Товары" }` |
| `ТипЗнч(Х)` | `Ty::Type` |
| `ЭтотОбъект` в модуле объекта справочника | `Ty::ThisObject { (Catalog, <имя>) }`; кооерсится в `MetadataRef { CatalogObject, … }` на входе в field / method lookup |
| параметр функции с JSDoc | результат lowering `TypeRef` из JSDoc |
| переменная после `Если ТипЗнч(Х) = Тип("Массив")` | `Ty::Array` внутри блока (narrowing) |

## Источники знаний о типах

`hir-ty` не владеет данными о типах — он оркестрирует lowering и
резолв через существующие источники. Новых параллельных таблиц в
`hir-ty` не создаётся.

| Источник | Что даёт | Как подключается |
|---|---|---|
| `bsl-platform::manager_methods_query` | Методы менеджеров (`СоздатьДокумент`, `ПустаяСсылка`, …) на все `MdoType` | Salsa-query; `hir::Type::manager()` и `method_lookup` читают результат |
| `bsl-platform::PlatformData::get_method` | Методы платформенных объектов (`Запрос`, `ТаблицаЗначений`, …) | `method_lookup::lookup_method` → `bsl-platform` |
| `bsl-metadata::MdoType::{from_plural, is_plural_form, manager_type_prefix}` | Маппинг `Документы` / `Справочники` / … → `MdoType` (RU/EN, case-insensitive) | Прямой вызов в `infer` |
| `bsl-metadata::metadata_object::StandardAttributeKind` | Стандартные реквизиты (с учётом Hierarchical / CodeLength / Periodicity) | `field_lookup::lookup_field` при lowering полей `MetadataRef` |
| `bsl-metadata::xml_parser::type_parser` | Парсинг XML-типов (`Справочник.Х`, `ОпределяемыйТип.Х`, `Composite`, `AnyRef`) | Выход — `AttributeType` → `TypeRef::from_attribute_type` → `TyLoweringContext::lower_type_ref` |
| `hir-def::ty::doc_types` | Парсер JSDoc | Выход — `TypeRef` → `TyLoweringContext` |
| `hir-def::resolver::Resolver` | Name → `PathResolution` / `Resolution` | Единая точка резолва для Definition- и Ty-слоёв |
| `ConfigsDatabase::configurations` | main + CFE extensions для файла | Salsa-input; `hir-ty` получает через trait |

## Архитектурные решения

1. **Доступ `hir-ty` к `Configuration` через trait `ConfigsDatabase`.**
   В `hir-ty` — абстрактный `ConfigsDatabase: DefDatabase` с методом
   `configurations(file_id) → Vec<VisibleConfig>`. Реализация в
   `ide-db` делегирует в `AnalysisProvider`. `hir-ty` не зависит от
   `ide-db` напрямую.

2. **`Resolver` итерирует по `visible_configurations`.**
   `resolve_cross_module` и `resolve_three_level_method` ходят по
   main → extensions в порядке, заданном provider'ом. Definition- и
   Type-слои видят одинаковый граф.

3. **Union-семантика для расширений CFE.** Методы одноимённых
   CommonModule / менеджерных модулей из main и extensions
   объединяются. При совпадении имени выигрывает расширение
   (соответствует runtime-семантике 1С).

4. **Универсальный `TyLoweringContext` на все источники XML-типов.**
   Реквизиты MDO, `Тип("СправочникСсылка.Х")`, `ОписаниеТипов`,
   JSDoc — одна pipeline: источник → `TypeRef` → `Ty`. Частных
   lowering'ов нет.

5. **Встроенные функции — `Scope::Builtins` в `Resolver`.** Shadowing
   builtin > var > module > workspace закреплён в `Resolver`, а не
   копипастится в `Semantics` и `infer`. Локальная переменная с
   именем встроенной функции не затеняет её вызов.

6. **Best-effort recovery для ERROR-узлов парсера.** Голое
   `Сп.В` на отдельной строке — не валидный BSL-statement, парсер
   обёртывает FIELD_EXPR в `NodeKind::Error`. HIR lowering
   (`hir-def::body::lower::stmt::try_lower_recovered_expr_stmt`)
   разворачивает well-formed expression-child ERROR-узла в
   `Stmt::Expr`, помечая получившиеся `ExprId` как recovered
   (`Body::is_recovered`). Это даёт `Semantics::type_of_expr`
   единую точку разрешения — он работает поверх `BodySourceMap`
   и корректно резолвит тип receiver'а, пока пользователь ещё
   печатает. Чтобы избежать «diagnostic flicker»:
   * `hir-ty::infer::push_inference_diagnostic` глотает
     `UnresolvedField/UnresolvedMethodCall/MismatchedArgCount/TypeMismatch`
     на recovered `ExprId`;
   * `cfg::builder::walk_statement_hir` пропускает recovered
     `Stmt::Expr`, чтобы dataflow/reachability не видели код,
     который пользователь ещё набирает.
   Инвариант: **единственный канал «syntax node → Ty» для IDE остаётся
   `Semantics::type_of_expr`**; recovered marker меняет только
   behaviour соседних консьюмеров, не контракт фасада.

7. **Platform-fallback для менеджеров и object/ref-контекстов.**
   `Справочники.Х.СоздатьЭлемент()` /
   `М = Справочники.Х; М.СоздатьЭлемент()` /
   `Спр.Записать()` после `Спр = … .СоздатьЭлемент()` резолвятся
   через `hir-ty::platform_manager_lookup`. Единая точка вызывается
   из двух путей: `infer_three_level_call` для 3-сегментного вызова
   и `method_lookup::lookup_method` для `Ty::ObjectManager` /
   `Ty::MetadataRef { *Object | *Ref, .. }` — чтобы алиасированный
   менеджер (`М = Справочники.Х`) не обходил фолбэк.
   * **Приоритет workspace → platform**: fallback срабатывает только
     на `Err(MethodNotFound)` от `Resolver::resolve_three_level_method`.
     `MethodNotExport` оставляем workspace-диагностикой (платформа не
     должна перезатирать ошибку видимости пользовательского модуля).
   * **Context-aware return type**: generic
     `PlatformMethod.return_type = "СправочникОбъект"` переписывается в
     `Ty::MetadataRef { CatalogObject, <mdo_name> }` через
     `map_generic_metadata_return_type` — таблица `(raw, MdoType) →
     MetadataKind` симметрична `MetadataKind::object_kind_for`.
   * **Гейт typo-safety**: 3-сегментный фолбэк требует, чтобы MDO
     был объявлен в хотя бы одной visible configuration (зеркалит
     `Resolver::mdo_visible_in_configs`) — иначе типичный опечаточный
     `Документы.НетТакогоДокумента.ПолучитьСсылку()` тихо завершался бы
     через platform data вместо честного `UnresolvedMethodCall`.
     В `lookup_method` на `Ty::ObjectManager` / `Ty::MetadataRef`
     такой гейт не нужен — receiver уже прошёл через
     `manager_lookup::lookup_manager_field`, который отвергает
     несуществующие MDO ещё до присвоения типа `ObjectManager`.

## Path-sensitive анализ (narrowing)

Narrowing типов через `Если ТипЗнч(Х) = Тип("…")` или
`Если Х <> Неопределено` — самостоятельный path-sensitive анализ
поверх CFG, отдельный от линейного `InferenceContext`. Реализован как
`hir_ty::narrow::NarrowingAnalysis` — латтис `Name → Ty` на основе
`dataflow::DataflowSolver`, с branch-aware transfer (`cfg::EdgeKind`).
Результат — оверлей `narrow_query(file_id, owner)`, который
`Semantics::type_of_expr` мерджит с базовым типом выражения.

Скоуп и принятые решения — в [`adr/ADR-01-narrowing.md`](adr/ADR-01-narrowing.md).
Feature flag `type_narrowing` в `bsl-analyzer.toml` даёт off-switch
для регрессий.

## Платформенные оверлеи и разрешение вызовов

Для корректного вывода типов в ситуациях, когда автоматическая экстракция данных платформы дает неполную или неточную картину, используются **курируемые оверлеи**.

### Платформенные оверлеи (Platform Overlays)

Оверлеи — это механизм внесения фактических исправлений в сигнатуры методов платформы.
- **Происхождение**: Файл `crates/bsl-platform/data/platform_overlays.json` (описывается в [`crates/bsl-platform/data/OVERLAYS.md`](../../crates/bsl-platform/data/OVERLAYS.md)).
- **Локальность**: Исправления применяются локально к конкретным методам. Оверлеи **не вводят** глобальных аксиом подтипирования (например, между HTML и DOM-элементами).
- **Владение и применение**: Слой `bsl-platform` владеет оверлеями и применяет их на этапе генерации: `build.rs` объединяет исправления с извлеченными данными платформы до того, как Rust-структуры будут сгенерированы. `hir-ty` потребляет уже скорректированные сигнатуры и выполняет разрешение вызовов. Оверлеи **не являются** отдельным источником кандидатов во время выполнения.

### Стадии разрешения кандидатов (Call Resolution)

При разрешении вызова метода или функции анализатор приводит каждый вызов к одному из трех взаимоисключающих состояний выбора:

1. **Сбор кандидатов**: Собираются все потенциальные сигнатуры из скорректированных метаданных платформы, встроенных функций, пользовательского кода и значений-функций. Каждый кандидат имеет стабильный семантический идентификатор `CandidateId` (`Platform`/`User`/`Builtin`/`FunctionValue` + слот сигнатуры), который определяет порядок сортировки независимо от порядка подачи на вход. Арность оценивается после сбора.
2. **Ранжирование и фильтрация**:
   - Каждый кандидат оценивается по аргументам вызова. Применимость ранжируется лексикографически: точные совпадения, присваиваемые преобразования (`Assignable`), коэрции, использованные значения по умолчанию, использование вариадического параметра. Чем меньше счет, тем лучше кандидат. Порядок в исходных данных не влияет на выбор.
   - **Unique**: После ранжирования ровно одна сигнатура — лучший известный подходящий кандидат. Её параметры используются для дальнейшей проверки.
   - **Ambiguous**: Либо несколько известных подходящих кандидатов делят минимальный счет и выживают, либо один или несколько неопределенных кандидатов выживают при отсутствии известных подходящих. Если выживает хотя бы один кандидат, `TypeMismatch` не генерируется.
   - **Rejected**: Ни один кандидат не выживает. Аргументы несовместимы со всеми arity-совместимыми сигнатурами, или ни один кандидат не подходит по арности. Состояние включает отклонения по арности и по типам.
3. **Генерация диагностик**: `TypeMismatch` генерируется только в состоянии **Rejected** и только для отклонения по типам. Сообщение строится по детерминированному наиболее подходящему отклоненному кандидату и его первому несовместимому аргументу.

Аргумент или параметр типа `Unknown` считается неопределенным и сам по себе не отклоняет вызов. Другие конкретные несовместимые аргументы по-прежнему могут привести к **Rejected**.

## Отклоненные альтернативы

Проект намеренно не использует два распространенных подхода к работе с неполнотой типов:

- **Глобальная решетка уверенности** (`Known | Recovered | Modeled | Unknown`). Такая решетка позволяет "подавить" диагностику, но скрывает реальные ошибки, когда источник факта недостаточно авторитетен. Вместо этого `hir-ty` принимает кандидатов с конкретными признаками применимости: `Applicable`, `Indeterminate`, `Incompatible`. `Indeterminate` не отклоняет вызов сам по себе, но и не позволяет замаскировать конкретную несовместимость.
- **Платформенные факты в `bsl-types::TypeKind`**. Типы платформы — не часть универсальной системы типов; они живут в `bsl-platform` и подаются в `hir-ty` через адаптеры. Это предотвращает размывание `TypeKind` сотнями платформенных сущностей и сохраняет чистоту слоя `bsl-types`.

## Ключевые инварианты

1. Один способ резолвить имя — `Resolver`, используется и в
   Definition, и в Ty-слое. Shadowing builtin > var > module встроен
   в `Resolver`.
2. Entities (`Ty`, `TypeRef`) не зависят от `db` / `salsa`.
3. Один публичный API для IDE — `hir::Semantics` + `hir::Type`.
4. Источники знаний о типах подключаются через trait-адаптеры, не
   через глобальные singleton'ы. `hir-ty` оркестрирует, не владеет
   данными.
5. Инвалидация — через Salsa inputs (`file_text`, `configurations`),
   без обходов кэша.
6. Диагностики типов коллектятся внутри lowering и inference и
   форвардятся единым каналом в `ide-diagnostics`.
7. `hir-ty` видит main + CFE через `visible_configurations`, не
   только текущий SourceRoot.

## Покрытие инвариантов

| # | Статус | Подтверждающие тесты / артефакты |
|---|---|---|
| 1 | ✅ | `Expr::Path` → `Resolver::resolve_name`; `Expr::New`, `Expr::Call(QualifiedPath)` (2 и 3 сегмента) → `TyLoweringContext` + `Resolver::resolve_qualified_method` / `resolve_three_level_method`. Регрессии: `crates/ide/tests/ide_suite/type_system_invariants.rs::single_resolver_cascade_*`. |
| 2 | ✅ | `TypeRef` и варианты `Ty` — plain data (`crates/hir-def/src/type_ref.rs`, `crates/hir-def/src/ty.rs`). |
| 3 | ✅ | Фасад `hir::Type` (`crates/hir/src/type_facade.rs`). `Semantics::type_of_expr(file_id, &SyntaxNode) → Ty` поверх `InferenceResult::expr_types_by_body`. Consumers мигрированы (`platform_completion`, `mdo_completion`). CI-гейт: `scripts/check-invariants.sh`. |
| 4 | ✅ | `ConfigsDatabase`, bridge `AttributeType → TypeRef`, `hir_ty::method_lookup::lookup_method`, `hir_ty::field_lookup::lookup_field`. `bsl_platform::manager_methods_query` вынесла последний IDE-side `PlatformData::instance()` за Salsa-gate. |
| 5 | ✅ | `db.infer` транзитивно зависит от `db.configurations`; тесты `infer_invalidation::infer_invalidates_when_config_set_changes`, `infer_three_level::three_level_invalidates_on_config_change`. |
| 6 | ✅ | `InferenceDiagnostic` (`UnresolvedMethodCall`, `MismatchedArgCount`, `TypeMismatch`, `UnresolvedField`, `ReadOnlyPropertyAssignment`, `RedundantAccessToObjectTwoLevel`, `MissedRequiredParameterCommonModule`) форвардятся через `ide-diagnostics/src/hir_inference_dispatch.rs`. `TypeMismatch` эмитится на всех call-path: qualified, 3-level, `Ty::Function`, fluent `Expr::Call { callee: Expr::Field }`. |
| 7 | ✅ | `ConfigsDatabase::configurations`, visibility-gate в `Resolver`, invalidation-тест. |

## Известные ограничения

- **`Движения.X.Добавить()`** в модулях документов. Поля регистров
  (dimensions / resources / attributes) резолвятся через
  `field_lookup::lookup_on_register`, но коллекция `Движения` как
  свойство на документе и `.Добавить()` → запись пока не
  смоделированы. Требует парсинг `<Recorders>` в `bsl-metadata` и
  новых вариантов `MetadataKind::Registrations { parent }` +
  `*RegisterRecord { parent }`.
- **Предопределённые элементы / значения перечислений** на
  `Ty::ObjectManager` (`Перечисления.Состояния.Активен`,
  `Справочники.Валюты.Доллар`). `hir_ty::manager_lookup` покрывает
  часть manager-side поверхности, предопределённые Catalog /
  ChartOf* возвращают `Ty::Unknown`.
- **SDBL-слой** (`sdbl-hir`) оперирует параллельной типовой системой
  (`SdblType`); миграция на общий `TypeRef` не входит в скоуп BSL
  type system.
- Narrowing-ограничения из [`ADR-01`](adr/ADR-01-narrowing.md):
  `ИЛИ`-composition в гардах, narrowing через границы вызовов,
  narrowing на `Попытка` / `Исключение`, `Х Есть Справочник`,
  cast-via-assignment.

## Чего нет в модели (в отличие от rust-analyzer)

- `Canonical<T>` — в BSL нет trait-based полиморфизма.
- Trait solver / `chalk` / `rustc_type_ir` — язык не имеет трейтов и
  дженериков.
- Autoderef, lifetime regions, `Projection`, `Placeholder` —
  семантически отсутствуют в BSL.

## Связанные документы

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — общая архитектура проекта.
- [`DATAFLOW.md`](DATAFLOW.md) — flow-sensitive анализ и
  инфраструктура CFG.
- [`adr/ADR-01-narrowing.md`](adr/ADR-01-narrowing.md) — narrowing
  scope, принятые решения.
