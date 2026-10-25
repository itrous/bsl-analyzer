# CLI-операции для baseline в PostgreSQL

Для централизованного поиска BSL Analyzer предоставляет набор команд под префиксом:

```bash
bsl-analyzer search baseline ...
```

## Конфигурация проекта

Минимальный TOML-пример для PostgreSQL-бэкенда:

```toml
[search.baseline]
backend = "postgres"

[search.baseline.postgres]
schema = "bsl_search"
url_env = "BSL_SEARCH_BASELINE_PG_URL"

[search.baseline.workspace_code.policy]
publish_branches = ["vendor", "develop"]

[[search.baseline.workspace_code.policy.branches]]
match = "vendor"
select_branch = "vendor"

[[search.baseline.workspace_code.policy.branches]]
match = "develop"
select_branch = "develop"
fallback_branch = "vendor"

[[search.baseline.workspace_code.policy.branches]]
match = "feature/*"
select_branch = "develop"
fallback_branch = "vendor"

[[search.baseline.workspace_code.policy.branches]]
match = "*"
select_branch = "develop"
fallback_branch = "vendor"

[search.baseline.reference]
snapshot_id = "reference:0.1.104"
```

Общая структура конфигурации проекта описана в
`docs/configuration/PROJECT_CONFIGURATION.md`. Здесь остаются только примеры,
связанные с search baseline.

Адрес PostgreSQL и схема берутся из конфигурации проекта (`url_env`, `schema`),
отдельных флагов для них у команд нет. Каталог проекта задаётся
`-s/--source-dir` (по умолчанию — текущий).

## Публикация снимков

### `publish`

Публикует snapshot в PostgreSQL.

```bash
BSL_SEARCH_BASELINE_PG_URL=postgres://shared-search \
bsl-analyzer search baseline publish \
  --source-dir ./my-project \
  --branch develop \
  --commit "$CI_COMMIT_SHA"
```

Для справки платформы:

```bash
BSL_SEARCH_BASELINE_PG_URL=postgres://shared-search \
bsl-analyzer search baseline publish \
  --corpus reference
```

Поддерживаемые полезные флаги:

- `--corpus workspace-code|reference`
- `--snapshot-id`
- `--branch`
- `--commit`
- `--parent-snapshot-id`
- `--allow-non-policy-branch`

## Просмотр и аудит

Команды просмотра собраны под `search baseline inspect`.

### `inspect list-snapshots`

Показать опубликованные snapshot'ы:

```bash
bsl-analyzer search baseline inspect list-snapshots --limit 20
```

Фильтры: `--corpus`, `--branch`, `--commit`.

### `inspect show-snapshot`

Показать один snapshot:

```bash
bsl-analyzer search baseline inspect show-snapshot --snapshot-id workspace-code:develop@abcdef
```

### `inspect list-file-objects`

Показать общие file objects:

```bash
bsl-analyzer search baseline inspect list-file-objects --limit 20
```

### `inspect show-file-object`

Показать один file object:

```bash
bsl-analyzer search baseline inspect show-file-object --file-object-id abcdef
```

### `inspect list-embeddings`

Показать список эмбеддингов:

```bash
bsl-analyzer search baseline inspect list-embeddings
```

### `inspect show-embedding-coverage`

Показать покрытие embeddings:

```bash
bsl-analyzer search baseline inspect show-embedding-coverage
```

## Администрирование

Команды под `search baseline admin` работают под ролью migrator.

### `admin migrate`

Создать или обновить схему хранения:

```bash
bsl-analyzer search baseline admin migrate --source-dir ./my-project
```

## Retention и GC

Порядок, блокировки, права и физическое освобождение места описаны в
[09-operations.md](09-operations.md#retention-компактация-и-освобождение-места).

### `inspect retention`

Проверить retention policy по всему каталогу snapshot'ов; `--limit` ограничивает
только число напечатанных снимков, сводка считается по всем:

```bash
bsl-analyzer search baseline inspect retention --source-dir ./my-project
```

### `admin prune`

Компактация цепочки и удаление просроченных snapshot'ов. Сначала dry-run — план и
оценка освобождаемого места, база не меняется:

```bash
bsl-analyzer search baseline admin prune --source-dir ./my-project
```

Затем выполнение:

```bash
bsl-analyzer search baseline admin prune --source-dir ./my-project --execute
```

### `admin gc`

Сборка осиротевших file objects, content objects и эмбеддингов. Сначала dry-run:

```bash
bsl-analyzer search baseline admin gc --source-dir ./my-project
```

Затем реальное удаление:

```bash
bsl-analyzer search baseline admin gc --source-dir ./my-project --execute
```

## Проверка конфигурации

Проверить, как конфиг разбирается и какой baseline будет выбран, можно так:

```bash
bsl-analyzer check-config --config ./bsl-analyzer.toml
```

Команда работает и для legacy JSON-конфига, если он ещё используется в проекте.
В выводе будет краткая сводка по:

- `source.root` и подключённым extensions;
- diagnostics / code lens / formatting;
- отключённым, явно включённым и параметризованным диагностическим правилам;
- workspace/reference baseline selection.

## Что смотреть в MCP-контуре

После настройки baseline полезно проверить `search(action=status)`:

- какой backend выбран;
- какой snapshot разрешился;
- не находится ли ветка в состоянии `stale` или `expired`;
- какие полнотекстовые и семантические источники реально используются;
- насколько готов прогрев семантического индекса и overlay.
