# BSL Analyzer

Экспериментальный Language Server для BSL (1С:Предприятие), написанный на Rust.

## Идея проекта

Проект проверяет гипотезу: что получит разработчик 1С, если применить
архитектуру [rust-analyzer](https://github.com/rust-lang/rust-analyzer) к
языку 1С:Предприятие — инкрементальные вычисления на Salsa, full-fidelity CST
на Rowan, многослойный HIR с выводом типов, flow-sensitive анализ через CFG и
dataflow.

Какие практические эффекты даёт такая архитектура:

- **Инкрементальные пересчёты.** Salsa переинвалидирует только зависимую от
  правки часть графа запросов, без полной переобработки проекта.
- **Семантический hover и completion.** Реальный вывод типов (HIR/Ty):
  `Документы.ПКО.<TAB>` фильтруется по типу receiver'а; hover показывает
  фактический тип выражения, включая union'ы из `ОписаниеТипов` и JSDoc.
  После `Если ТипЗнч(Х) = Тип("Массив")` тип внутри ветки сужается (narrowing).
- **Find references и rename по символам.** Поиск работает через
  `SemanticSymbol`, а не текстовый grep — одноимённые локальные переменные и
  экспорты модуля не путаются.
- **Flow-sensitive диагностики.** Unreachable code, неиспользованные
  присваивания, потерянные `Возврат` ищутся через CFG и reaching definitions.
- **Анализ незаконченного кода.** Recovery-узлы Rowan позволяют hover и
  completion работать на полу-набранной строке без flicker'а диагностик.

Подробнее — в [`docs/architecture/ARCHITECTURE.md`](docs/architecture/ARCHITECTURE.md)
и [`docs/architecture/TYPE_SYSTEM.md`](docs/architecture/TYPE_SYSTEM.md).

## Возможности

- Набор диагностик качества кода BSL
- LSP — поддержка Language Server Protocol для IDE
- Подсветка структуры комментариев методов: разделы, параметры, поля, типы и ссылки
- MCP — встроенный сервер Model Context Protocol для AI-агентов
- SonarQube — отчёты SARIF и JSONL для CI/CD
- Конфигурация проекта через `bsl-analyzer.toml`
- Linux, Windows, macOS (Apple Silicon)

## Поддерживаемая версия платформы

Проект разрабатывается и тестируется только на **1С:Предприятие 8.3.27**.
Полнота глобального контекста аттестована по каталогу EDT этой версии, а
справка платформы — типы, методы, описания — читается при запуске из
установленной платформы или пакета справки, а без них по умолчанию — сохранённый
снимок прежней установки, если он есть, иначе встроенные факты интерфейса без
описаний; сетевой корпус подключается явно:
`source = "external"` с `url` или `path` (`[platform_help]`, см.
[`docs/configuration/PROJECT_CONFIGURATION.md`](docs/configuration/PROJECT_CONFIGURATION.md)
и [`crates/bsl-platform/data/PROVENANCE.md`](crates/bsl-platform/data/PROVENANCE.md);
как собрать пакет справки и подключить его — [`docs/platform-help.md`](docs/platform-help.md)).

Более ранние версии и режимы совместимости не проверяются: анализ и
рекомендации диагностик исходят из того, что доступен весь API 8.3.27.

## Сообщество

Вопросы, идеи, баги — в Telegram-чат: https://t.me/+K7CcQPNZobE3YjUy

## Установка

### Расширение для редактора (VS Code / Cursor / VSCodium / Zed)

Самый простой способ получить подсветку, навигацию и диагностики прямо в редакторе —
поставить расширение `bsl-analyzer-lsp`:

- **VS Code Marketplace** (VS Code): <https://marketplace.visualstudio.com/items?itemName=ElishevKirill.bsl-analyzer-lsp>
- **Open VSX** (Cursor, Windsurf, VSCodium и другие редакторы на Open VSX): <https://open-vsx.org/extension/ElishevKirill/bsl-analyzer-lsp>
- **Zed** (расширение `bsl`): <https://zed.dev/extensions/bsl>

Для CLI-анализа и MCP-интеграции с AI-агентами поставьте бинарник ниже.

Подсветка комментариев методов работает через semantic tokens: разделы
`Параметры` / `Parameters`, `Возвращаемое значение` / `Returns`, примеры и
варианты вызова, имена параметров и вложенных полей, типы и ссылки `См.` / `See`.
Цвета задаёт тема редактора; в VS Code должна быть включена настройка
`editor.semanticHighlighting.enabled`. Подсветка показывает структуру описания,
но сама по себе не подтверждает существование указанного типа или метода.

В подсказке при наведении одиночный тип и его описание располагаются рядом с
именем параметра. Несколько вариантов типа и поля возвращаемой структуры показаны
вложенными списками; маркеры `*`, `**` и глубже задают уровни полей. Перенесённые
строки описаний и типов остаются у своего параметра или поля; примеры выводятся
отдельным блоком кода с сохранением отступов и пустых строк.

### Linux / macOS

<!-- INSTALL_URL:github -->
```bash
curl -fsSL https://raw.githubusercontent.com/itrous/bsl-analyzer/develop/scripts/install.sh | bash
```

Или с указанием версии:

```bash
curl -fsSL https://raw.githubusercontent.com/itrous/bsl-analyzer/develop/scripts/install.sh | bash -s -- --version 0.1.38
```
<!-- /INSTALL_URL -->

### Windows (PowerShell)

<!-- INSTALL_URL_WINDOWS:github -->
```powershell
irm https://raw.githubusercontent.com/itrous/bsl-analyzer/develop/scripts/install.ps1 | iex
```

Или с указанием версии:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/itrous/bsl-analyzer/develop/scripts/install.ps1))) -Version <version>
```
<!-- /INSTALL_URL_WINDOWS -->

Инсталлятор кладёт `bsl-analyzer.exe` в `%LOCALAPPDATA%\Programs\bsl-analyzer` и
добавляет каталог в пользовательский `PATH` (отключается ключом `-NoPathUpdate`).

## Использование

> **Запуск из интеграций — прямой вызов бинарника, без shell-обёрток.**
> `bsl-analyzer` — обычный исполняемый файл, и режимы задаются его подкомандами
> (`lsp`, `analyze`, `mcp serve`). Не запускайте через `CreateProcess` шелл-скрипты:
> `install.sh` — это установщик, а не точка входа, и на Windows такой вызов даёт
> `os error 193` («не является приложением Win32»). Если обёртка на Windows всё же
> нужна, она обязана быть `.cmd`, `.bat` или `.exe`.

### LSP-сервер

```bash
bsl-analyzer lsp
```

### Анализ проекта

```bash
# Консольный вывод
bsl-analyzer analyze -s ./my-project

# SARIF-отчёт
bsl-analyzer analyze -s ./my-project -r sarif -o ./reports

# JSONL-вывод
bsl-analyzer analyze -s ./my-project --format=jsonl > report.jsonl
```

### Интеграция с AI через MCP

BSL Analyzer включает встроенный [MCP-сервер](https://modelcontextprotocol.io/) (Model Context Protocol), который позволяет AI-агентам работать с кодом 1С, справкой платформы, SDBL-запросами и отладкой.

Самый простой способ подключить инструменты к AI-клиентам — выполнить автоустановку:

```bash
# Устанавливает global reference и project workspace
bsl-analyzer mcp install \
  --target all \
  --preset recommended \
  --source-dir ./my-project
```

Подробности:

- [Настройка MCP и профилей](docs/mcp/README.md)
- [Инструменты AI и расширение для 1С](docs/mcp/TOOLS_AND_EXTENSION.md)

## Куда идти дальше

`README.md` остаётся короткой продуктовой точкой входа: установка, запуск и
основные сценарии. Детальная навигация по документации собрана в
`docs/README.md`.

Быстрые ссылки:

- [Карта документации](docs/README.md) — полный обзор пользовательских и внутренних документов
- [Конфигурация проекта](docs/configuration/PROJECT_CONFIGURATION.md) — структура `bsl-analyzer.toml`
- [Конфигурация диагностик](docs/configuration/DIAGNOSTICS.md) — параметры диагностических правил
- [Настройка MCP](docs/mcp/README.md) — профили, `mcp install` и ручной запуск
- [Архитектура](docs/architecture/ARCHITECTURE.md) — устройство анализатора и основные пайплайны

## Конфигурация

Основной файл настройки проекта — `bsl-analyzer.toml` в корне репозитория.

Минимальный пример:

```toml
[source]
# Основная конфигурация в самом репозитории:
root = "src/cf"
extensions = ["Расширения/ОсновноеРасширение"]

[diagnostics]
ordinaryAppSupport = false
dataflowMaxIterations = 10000

[diagnostics.parameters]
CommentedCode = false
BadWords = true

[diagnostics.parameters.CyclomaticComplexity]
complexityThreshold = 20
```

Если основная конфигурация лежит не в репозитории, а в общем на машину хранилище
выгрузок, вместо `root` задаётся зависимость:

```toml
[source]
extensions = ["Расширения/ОсновноеРасширение"]

[source.configuration]
id = "UT11"
version = "11.5.22.129"
```

Путь к хранилищу берётся из `ONEC_CONFIGURATIONS_ROOT`: сначала из окружения
процесса, затем из `.env` рядом с `bsl-analyzer.toml`. Относительное значение
отсчитывается от корня проекта. Проекту, которому основная конфигурация не нужна,
можно не задавать ни `root`, ни `[source.configuration]`.

Общая структура файла описана в `docs/configuration/PROJECT_CONFIGURATION.md`,
а параметры диагностических правил — в `docs/configuration/DIAGNOSTICS.md`.

## Сборка из исходников

**Требования:** Rust 1.95+

```bash
git clone https://github.com/itrous/bsl-analyzer.git
cd bsl-analyzer
cargo build --release
```

Если вы планируете менять код проекта, процесс контрибуции и профильная
документация для разработчиков собраны в `CONTRIBUTING.md` и `docs/README.md`.

## Архитектура проекта

```text
bsl-analyzer (LSP/CLI/MCP)
    └── ide
        ├── ide-diagnostics
        ├── ide-assists
        └── ide-db
            └── hir / hir-def / hir-ty
                └── syntax / parser / lexer
```

Подробнее об архитектуре, Salsa, dataflow и centralized search — в
`docs/architecture/ARCHITECTURE.md`, `docs/architecture/DATAFLOW.md` и
`docs/central-postgres-search/README.md`.

## Благодарности

- [rust-analyzer](https://github.com/rust-lang/rust-analyzer) — архитектурный референс: Salsa, Rowan, слойная организация `syntax → hir-def → hir-ty → ide`, подход к инкрементальной компиляции и LSP-фичам. Бóльшая часть структуры крейтов в `bsl-analyzer` следует именно этому образцу.
- [bsl-language-server](https://github.com/1c-syntax/bsl-language-server) — статический анализатор BSL (Java, LGPL-3.0); источник формализации диагностических правил и стандартов качества кода 1С.
- [1c-syntax](https://github.com/1c-syntax) — сообщество разработчиков инструментов для 1С.
- [RDT1C](https://github.com/tormozit/RDT1C) — «Инструменты разработчика» для 1С; использован как референс для модуля `naparnik`, а также для модели ранжирования автодополнения (приоритет fuzzy-качества над категорией, контекстный буст по типу) и реестра шаблонов кода.

## Лицензия

Лицензия задаётся по крейтам: карта тиров, условия повторного использования
и сторонние материалы со своими условиями — в `LICENSING.md`, история
происхождения и благодарности — в `NOTICE`, подробности — в `docs/legal/`.
