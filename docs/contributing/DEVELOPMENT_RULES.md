# Правила разработки

Этот документ описывает именно инженерные правила: как выбирать тип
диагностики, как писать тесты, на что смотреть в производительности и как не
ломать существующий пайплайн. За процесс контрибуции, Merge Request и релизы
отвечают отдельные документы:

- `CONTRIBUTING.md`
- `docs/contributing/LOGGING.md`
- `docs/contributing/VERSIONING.md`
- `docs/contributing/SALSA_GUIDE.md`

## Архитектура диагностик

### Выбор типа диагностики

| Тип | Когда использовать | Сигнатура | Пример |
|-----|-------------------|-----------|--------|
| **HIR-based** | Семантика: return, присваивания, deprecated, транзакции | `from_hir(range, ctx)` | UnreachableCode, SelfAssign |
| **CFG/Dataflow** | Flow-sensitive: все пути, reaching definitions | `from_hir(range, method_id, ctx)` | AllFunctionPathMustHaveReturn |
| **AST-based** | Синтаксис: форматирование, паттерны в коде | `check(ctx)` | DoubleNegatives, MagicDate |
| **SDBL-based** | Запросы: таблицы, поля, алиасы | `check(ctx)` + `sdbl_hir_in_file()` | AssignAliasFieldsInQuery |
| **Metadata** | Бизнес-правила 1С: имена модулей, контексты | `from_metadata(metadata, config)` | CommonModuleNameClient |
| **Text-based** | Текст: длина строк, пустые строки | `check(ctx)` | LineLength, ConsecutiveEmptyLines |

### Критерии выбора

```
Нужна информация о потоке выполнения (все пути, использование до определения)?
  → CFG/Dataflow-based

Проверка собирается при построении HIR (return, deprecated, транзакции)?
  → HIR-based (добавить в BodyDiagnostic)

Нужны метаданные 1С (Configuration, CommonModule)?
  → Metadata-based

Анализ SDBL запросов?
  → SDBL-based

Проверка синтаксических паттернов без семантики?
  → AST-based

Проверка текста (не AST)?
  → Text-based
```

### Структура файлов

```
crates/hir-def/src/body.rs              ← BodyDiagnostic enum
crates/hir-def/src/body/lower/*.rs      ← Сбор HIR диагностик при lowering
crates/ide-diagnostics/src/lib.rs       ← Dispatch
crates/ide-diagnostics/src/handlers/    ← Один файл = одна диагностика
```

### Шаблоны

**HIR-based (предпочтительный для семантики):**
```rust
// handlers/my_diagnostic.rs
pub fn from_hir(range: TextRange, ctx: &DiagnosticsContext) -> Option<Diagnostic> {
    if ctx.config.is_disabled(DiagnosticCode::MyDiagnostic) {
        return None;
    }
    Some(Diagnostic {
        code: DiagnosticCode::MyDiagnostic,
        message: "...".into(),
        severity: Severity::Warning,
        range,
        tags: vec![],
        fixes: vec![],
    })
}
```

**AST-based:**
```rust
pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    if ctx.config.is_disabled(DiagnosticCode::MyDiagnostic) {
        return Vec::new();
    }
    let root = ctx.parse().syntax_node();
    // Один проход по descendants()
    root.descendants()
        .filter_map(|node| check_node(&node))
        .collect()
}
```

### Производительность

**Обязательно:**
- Один проход по AST (не вложенные `descendants()`)
- Early exit при `is_disabled()`
- Дешёвые проверки перед дорогими

**Запрещено:**
```rust
// ❌ O(n²) - вложенные descendants
for node in root.descendants() {
    for child in node.descendants() { ... }
}

// ❌ Множественные обходы
let has_a = root.descendants().any(|n| ...);
let has_b = root.descendants().any(|n| ...);
```

**Правильно:**
```rust
// ✅ Один проход, множественные проверки
let nodes: Vec<_> = root.descendants().collect();
let has_a = nodes.iter().any(|n| ...);
let has_b = nodes.iter().any(|n| ...);
```

---

## Критичные правила

### 1. Тестирование

```rust
// ✅ Helper методы для позиций
assert_diagnostic_range(&code, &diag, line, start_col, end_col);
assert_diagnostic_range_multiline(&code, &diag, start_line, start_col, end_line, end_col);

// ❌ Магические числа
assert_eq!(diag.range, TextRange::new(42.into(), 156.into()));
```

```bash
cargo test --all
UPDATE_EXPECT=1 cargo test  # Обновить snapshots после анализа
```

### 2. Качество кода

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

### 3. Логирование

- Для отладки и телеметрии используйте `tracing`.
- Не используйте `println!`, `dbg!` и `eprintln!` как механизм штатного
  логирования.
- Соглашения по уровням, `span`, `BSL_LOG`, `BSL_PROFILE` и другим переменным
  окружения описаны в `docs/contributing/LOGGING.md`.

### 4. Документация

- Код должен быть самодокументируемым
- Комментарии только для "почему", не "что"
- TODO с автором и issue: `// TODO(username): #123`

### 5. Библиотеки

Перед использованием crate → Context7 (`resolve-library-id` + `query-docs`)

### 6. Отменяемость LSP-запросов

Salsa сам проверяет отмену на входе в каждый запрос, поэтому «глухие зоны» —
только длинные циклы/fixpoint внутри **одного** запроса или в IDE-слое над
запросами.

- Любой новый LSP-обработчик класса «дорогой/многофайловый» (обход графа
  вызовов, скан всех модулей, массовая правка файлов — `rename`,
  `callHierarchy`, `workspace/symbol`) идёт через `on_latency` и вызывает
  `db.unwind_if_revision_cancelled()` в каждом цикле по файлам/узлам. Так
  `$/cancelRequest` и неявная отмена при записи (didChange) размотают работу, а
  не досчитают её впустую.
- Однофайловым обработчикам (`hover`, `completion`, `semanticTokens`, …)
  отдельные точки не нужны — им хватает автоматических проверок salsa на
  границах запросов. Вывод аудита текущих `on_latency`-обработчиков
  (`crates/bsl-analyzer/src/handlers/request.rs`): все однофайловые, кроме
  `textDocument/references` (свип по source root) — он уже соблюдает правило,
  вызывая `unwind_if_revision_cancelled()` в `crates/ide/src/references.rs` и
  `crates/hir-def/src/name_usage_index.rs`.
- Отмена размывается в `-32800` (`RequestCanceled`) через
  `salsa::Cancelled::catch` в `run_latency_handler`; паника (не отмена) остаётся
  `InternalError`. Тип отмены (`local` vs `pending-write`) и elapsed логируются
  там же.

---

## Контексты диагностик: `AnalysisContext`, `DiagnosticsContext`, `BodyContext`

Единица пересчёта диагностик — метод, поэтому контекстов три, и выбор
хендлера начинается с вопроса «что он читает».

- `AnalysisContext` — позиционно-свободное: конфиг, метаданные, кросс-файловое,
  `module_interface()` (объявления модуля без позиций), per-method dataflow
  (`method_cfg`, `reaching_definitions`, `method_path_terminates`, …).
- `DiagnosticsContext` (Deref → `AnalysisContext`) — файловое позиционное:
  `parse()`, `file_text()`, `item_tree()`, `symbol_tree()`, `module_bodies()`,
  `call_summary()`, `line_index()`. Пересчитывается на каждую правку файла.
- `BodyContext` (Deref → `AnalysisContext`) — одно тело: `body()`,
  `source_map()`, `root()` (оторванный узел метода либо корень файла для
  модульного кода), `nodes()`/`tokens()` (у модульного кода — без поддеревьев
  методов), `line_index()` по тексту тела, `decl()`, `method_name_range()`,
  `cfg()`/`reaching_definitions()`/`hir_metrics()`. Диапазоны здесь —
  `LocalRange`, относительно `root()`; свод `file_diagnostics` поднимает их
  в файл сам.

Формы хендлера:

```rust
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>)   // по телу — мемо на метод
pub fn from_hir(range: LocalRange, ctx: &AnalysisContext) -> Option<Diagnostic<LocalRange>> // диспетчер HIR по телу
pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic>                      // файловый
```

Правило выбора: если проверка читает только своё тело и `module_interface`,
это `check_body` — она регистрируется в `BODY_DIAGNOSTICS` (`body.rs`) и не
пересчитывается на чужие правки. Если ей нужен `parse()`, `item_tree`,
`call_summary`, соседство токенов через границу метода или строки файла
целиком (`LineLength`, `MissingSpace`, `IncorrectLineBreak`, `CommentedCode`),
это файловый `check` в `runner.rs`. Файловый хендлер, вызывающий per-method
запросы по каждому методу, — переходное состояние, а не форма: он платит N
выборок на правку.

Инвариант per-method мемо — позиционная независимость: `check_body` не должен
доставать файловое состояние окольным путём (через провайдер, `symbol_tree`,
`module_bodies`). Утечку ловит гейт I1 (`crates/ide/tests/method_increment_churn.rs`:
`execute == 1` на правку тела) — прогонять его при добавлении нового чтения в
`BodyContext`/`AnalysisContext`.

---

## Чек-лист перед коммитом

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

- [ ] Нет `println!/dbg!/eprintln!`
- [ ] Нет магических чисел в assert для TextRange
- [ ] Нет O(n²) паттернов
- [ ] Early exit при `is_disabled()`
- [ ] Тесты покрывают positive и negative cases

---

## Справка платформы 1С

Полный корпус справки платформы (типы, методы, свойства, конструкторы,
глобальные функции **с описаниями**) в репозитории **не хранится** и в сборку
не входит (#312). Анализатор читает его при запуске из источника
`[platform_help]` (`docs/configuration/PROJECT_CONFIGURATION.md`):
установленная платформа (HBK читаются собственным кодом, без 7z и внешних
программ), пакет справки, `auto` (умолчание; без установленной платформы —
сохранённый снимок, а без него встроенные факты; сети не касается) или `none`. `build.rs` генерирует только каталог EDT; `BSL_PLATFORM_PATH`
задаёт каталог HBK для `installed`/`auto` при запуске.

В сборку входят **встроенные факты интерфейса** —
`crates/bsl-platform/data/platform_facts.json` (#326): тот же корпус без
единого текстового поля (нет `description`, `param_descriptions`, `examples`,
`notes`, `see_also`, `keywords`), с именами, сигнатурами, параметрами, типами,
версиями и контекстами. Их отдают `source = "bundled"`, библиотечное
использование без `BSL_PLATFORM_HELP_CORPUS` и последняя ступень `auto`, когда
полный корпус недоступен; `none` по-прежнему пуст. Генерация —
`scripts/strip-help-corpus-texts.py`, структурная проверка —
`crates/bsl-platform/tests/bundled_facts.rs`; обе работают по allow-list ключей,
так что текстовое поле под любым именем не пройдёт. Происхождение и правовой
статус — `crates/bsl-platform/data/PROVENANCE.md`.

### Тесты и корпус

Тесты, проверяющие факты справки, помечены
`#[cfg_attr(not(corpus_contract), ignore = "corpus contract: …")]`:

- **автономный профиль** (обычный `cargo test --workspace`, pre-commit hook,
  CI без 1С) их не запускает — в отчёте они видны как `ignored`, остальные
  тесты работают на встроенных фактах интерфейса и собственных фикстурах;
- **corpus-contract профиль** запускает их на закреплённом корпусе — прежнем
  `platform_data.json` (SHA-256
  `3c759994cbd82a1522b1c497d9f2ef68c77b776d5c6723ba5a72623b570cacce`, последний
  коммит с файлом — `af94692c`). Без входа профиль падает, а не пропускает:

```bash
corpus="$HOME/.cache/bsl-analyzer/platform-help-corpus/platform_data.json"
mkdir -p "$(dirname "$corpus")"
git show af94692c:crates/bsl-platform/data/platform_data.json > "$corpus"
sha256sum "$corpus"   # 3c759994…cacce

CARGO_TARGET_DIR=target-corpus RUSTFLAGS="--cfg corpus_contract" \
  BSL_PLATFORM_HELP_CORPUS="$corpus" cargo test --workspace
```

В CI корпус не скачивается, и профиль там не запускается: тесты с
`corpus_contract` остаются `ignored`, а запускать их нужно на машине, где корпус
есть. Job `corpus-contract` проверяет другое — текстовый путь загрузчика справки
на синтетической фикстуре `crates/bsl-platform/tests/fixtures/help/corpus.json`
(выдуманные описания, описания параметров и примеры) и встроенные факты.
Каждый вызов `cargo test` в нём обязан выполнить хотя бы один тест, поэтому
набор из одних `ignored` валит job.

`BSL_PLATFORM_HELP_CORPUS` указывает corpus JSON для процесса, которому
никто не выбрал источник: библиотечный код в тестах и приложение без
`[platform_help]`. Отдельный `CARGO_TARGET_DIR` нужен, потому что
`RUSTFLAGS` пересобирает всё.

### Обновление корпуса справки

При выходе новой версии платформы или исправлении извлечения готовится новый
пакет справки (см. «Пакет справки для публикации» ниже) и публикуется
владельцем хранилища; закреплённый вход corpus-contract меняется осознанно —
вместе с `PINNED_CORPUS_SHA256` и дайджестом наблюдаемой поверхности в
`crates/bsl-platform/tests/help_corpus_equivalence.rs`.

### Входы из Docker-платформы и реальный smoke

На машине без установленной 1С неизменённые HBK берутся из контейнера
платформы; Docker нужен только для копирования входов:

```bash
container=rtools-1c-itrous
platform_dir=/opt/1cv8/x86_64/8.3.27.2214
hbk_dir="$HOME/.cache/bsl-analyzer/platform-help-smoke/8.3.27.2214"
mkdir -p "$hbk_dir"
docker cp "$container:$platform_dir/shcntx_ru.hbk" "$hbk_dir/shcntx_ru.hbk"
docker cp "$container:$platform_dir/shlang_ru.hbk" "$hbk_dir/shlang_ru.hbk"
docker exec "$container" sha256sum "$platform_dir/shcntx_ru.hbk" "$platform_dir/shlang_ru.hbk"
sha256sum "$hbk_dir/shcntx_ru.hbk" "$hbk_dir/shlang_ru.hbk"

BSL_PLATFORM_HELP_SMOKE_DIR="$hbk_dir" \
  cargo test --release -p platform-help --test real_hbk_smoke -- --ignored --nocapture
```

Smoke читает оба реальных HBK собственным reader'ом, проверяет режим
`installed` на чистом кэше (`Массив.Добавить` по RU/EN и непустое описание),
повторное использование кэша и отказ на испорченной копии каждого файла. Без
входа он падает, а не пропускается; в обычном прогоне тестов он `ignored`.

### Пакет справки для публикации

Пакет — каталог из `platform_data.json` (корпус без оверлеев: оверлеи
накладывает анализатор при загрузке), `manifest.json` (`schema_version`,
`corpus_id`, версия платформы, версия экстрактора, SHA-256 корпуса) и
`NOTICE.md` о принадлежности текстов ООО «1С-Софт». Готовит его та же
библиотека, без внешних утилит:

```bash
# из HBK установленной платформы (или копий из Docker-платформы)
bsl-analyzer-app platform-help package --hbk-dir "$hbk_dir" \
  --corpus-id platform-help-8.3.27.2214 -o "$HOME/platform-help-8.3.27.2214"

# из уже имеющегося корпуса JSON (как есть)
bsl-analyzer-app platform-help package --corpus platform_data.json \
  --corpus-id <имя> [--platform-version <версия>] -o "$HOME/<имя>"
```

Каталог назначения не должен существовать; корпус, который анализатор
отверг бы, пакетом не становится. Публикация — отдельное действие владельца
хранилища (рекомендовано: отдельный репозиторий `itrous/bsl-platform-help`,
release на версию корпуса): три файла пакета загружаются как assets одного
release, и пользователи указывают
`url = "https://github.com/itrous/bsl-platform-help/releases/download/<tag>/manifest.json"`
— корпус берётся из того же каталога, что и манифест. Анализатор сам ничего не
публикует.

