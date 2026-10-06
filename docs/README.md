# Документация BSL Analyzer

`README.md` в корне репозитория отвечает за быстрый старт и основные сценарии
использования. Этот файл — карта остальных документов: здесь удобнее искать
детали по конфигурации, архитектуре, MCP и разработке самого проекта.

## Быстрый старт

- Пользователям: `../README.md`
- Общая конфигурация проекта: `configuration/PROJECT_CONFIGURATION.md`
- Настройка MCP и AI-инструментов: `mcp/README.md`
- Инструменты MCP и интеграция с 1С: `mcp/TOOLS_AND_EXTENSION.md`
- Конфигурация диагностик: `configuration/DIAGNOSTICS.md`

## Для пользователей

- `../README.md` — установка и базовые сценарии CLI
- `configuration/PROJECT_CONFIGURATION.md` — структура `bsl-analyzer.toml`
- `configuration/DIAGNOSTICS.md` — параметры диагностик и правила включения / отключения
- `CI_REPORTERS.md` — форматы отчётов `analyze` для CI (SARIF, GitLab Code Quality, JUnit)
- `METADATA_COMPATIBILITY.md` — ограничения и совместимость загрузки метаданных

## MCP и AI-интеграции

- `mcp/README.md` — выбор профиля, `mcp install`, scope'ы и ручной запуск
- `mcp/TOOLS_AND_EXTENSION.md` — матрица tools, prerequisites и расширение 1С для live-инструментов

## Для контрибьюторов

- `../CONTRIBUTING.md` — процесс контрибуции и базовые ожидания по качеству
- `contributing/DEVELOPMENT_RULES.md` — правила по коду, тестам и диагностическим обработчикам
- `contributing/VERSIONING.md` — политика релизов и тегов
- `contributing/LOGGING.md` — логирование и профилирование
- `contributing/SALSA_GUIDE.md` — практические заметки по Salsa и инкрементальным вычислениям

## Стабильные архитектурные справки

- `architecture/ARCHITECTURE.md` — обзор слоёв, крейтов и основных пайплайнов
- `architecture/DATAFLOW.md` — устройство dataflow-подсистемы и текущий статус миграции диагностик
- `architecture/MERGE_AUDIT_ASSISTANT.md` — proposal по умному merge-аудиту обновлений и сравнению объектов 1С

## Архитектурные заметки

- [BA-023: платформенная проверка полного BSL-модуля](architecture/BA-023-MODULE-SYNTAX-CHECK.md) — контексты компиляции, нативный worker, ограничения и результаты приёмки `execute(check)`
- `architecture/SEARCH_BASELINE_OVERLAY.md` — описание модели `baseline + overlay`
- `central-postgres-search/README.md` — набор документов по централизованному поиску в PostgreSQL

## Roadmap и плановые документы

- `roadmap/README.md` — индекс активных направлений
- `roadmap/type-inference.md`, `roadmap/lsp-features.md`, `roadmap/workspace-symbols.md`, `roadmap/name-index*.md` — текущие планы по фичам и инфраструктуре
