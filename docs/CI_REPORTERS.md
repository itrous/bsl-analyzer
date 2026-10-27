# Репортеры для CI

`bsl-analyzer analyze` умеет писать отчёты в нескольких форматах через флаг
`-r/--reporters` (можно перечислить несколько через запятую). Файлы пишутся в
каталог `-o/--output-dir`.

| Ключ          | Файл                            | Назначение                                  |
|---------------|---------------------------------|---------------------------------------------|
| `console`     | — (stdout)                      | Сводка по числу файлов и замечаний          |
| `json`        | `bsl-json.json`                 | Полный машиночитаемый отчёт                  |
| `sarif`       | `bsl-analyzer.sarif`            | SARIF 2.1.0 (GitHub code scanning и др.)     |
| `codequality` | `gl-code-quality-report.json`   | GitLab Code Quality (виджет в MR)           |
| `junit`       | `bsl-analyzer.junit.xml`        | JUnit XML (вкладка «tests» в CI)            |

## Координаты находки

Анализатор считает место находки от НУЛЯ, и репортеры расходятся в том, переводят ли они
его в привычную человеку нумерацию с единицы.

| Выход | Строки | Колонки | Конец диапазона |
|---|---|---|---|
| `--format jsonl` | 0-based | 0-based, в СИМВОЛАХ (кодовых точках Unicode) | не включается |
| `sarif` | 1-based | 1-based, в символах | не включается |
| `junit` | 1-based (имя теста и текст `<failure>`) | 1-based (имя теста) | — |
| `codequality` | 1-based (`location.lines.begin`) | не выводятся | — |
| `json`, `console` | не выводятся (только число находок по файлам) | не выводятся | — |
| инструменты MCP, `location.range` | 0-based | 0-based, в кодовых единицах UTF-16 | не включается |
| инструменты MCP, устаревший `range` у `diagnostics` и находки `query` | 0-based | 0-based, в символах | не включается |

Полный диапазон находки (строки и колонки начала и конца) отдают `--format jsonl` и
`sarif`; `junit` и `codequality` дают только строку начала (`junit` — ещё колонку начала
в имени теста); репортеры `json` и `console` позиций не содержат вовсе.

Строки делятся так же, как в LSP: переводом строки считаются `\n`, `\r\n` и одиночный `\r`.
В файле с одиночными `\r` нумерация по одному `\n` (как у `git blame` или `str::lines`
в Rust) уезжает на строку за каждый такой `\r`.

Отсюда правило чтения `jsonl`: строка начала — строка номер `start_line` (с нуля), строка
конца — `end_line`; `start_column` отсчитывается в строке начала, `end_column` — в строке
конца. У однострочной находки (`start_line == end_line`) цель — ровно срез
`строка[start_column:end_column]`; у многострочной — от `start_column` строки начала до
`end_column` строки конца, а `end_column == 0` значит, что находка кончается переводом
строки перед строкой `end_line`.

Единица колонки — **символ**, а не байт. В строке с кириллицей байтовое смещение примерно
вдвое больше символьного, поэтому расчёт «по байтам» совпадает с ответом на коротких отступах
и расходится тем сильнее, чем дальше находка от начала строки. У инструментов MCP единица
третья — кодовые единицы UTF-16 (`docs/mcp/LOCATION_CONTRACT.md`); от символьной она отличается
только на символах вне BMP.

## GitLab Code Quality

GitLab показывает такой отчёт виджетом прямо в merge request, аннотируя
новые замечания в диффе. Дифф между base- и head-пайплайном считается по
`fingerprint`, который у нас устойчив к сдвигу строк (хеш от пути, кода правила
и нормализованной строки-источника, а не от номера строки) — вставка кода выше
замечания не помечает его как новое.

```yaml
bsl-analyze:
  stage: test
  script:
    - bsl-analyzer-app analyze -s src -r codequality -o reports -q
  artifacts:
    reports:
      codequality: reports/gl-code-quality-report.json
    paths:
      - reports/gl-code-quality-report.json
    when: always
```

## JUnit XML

Там, где Code Quality недоступен, JUnit-отчёт рендерится нативной вкладкой
«tests»: каждый файл — `<testsuite>`, каждое замечание — упавший `<testcase>` с
кодом правила в `type` и `path:line` в теле `<failure>`.

```yaml
bsl-analyze:
  stage: test
  script:
    - bsl-analyzer-app analyze -s src -r junit -o reports -q
  artifacts:
    reports:
      junit: reports/bsl-analyzer.junit.xml
    when: always
```

Оба репортера можно включить одновременно: `-r codequality,junit`.

Оба формата совместимы с потоковым и инкрементальным режимами `analyze`
(`--incremental`, `--changed-files`, `--git-diff`).

## Шлюз по базовой линии

После добавления `[diagnostics.baseline]` рекомендуемый порядок миграции такой:

```bash
bsl-analyzer-app diagnostics baseline create -s .
# проверить и добавить файл базовой линии в систему контроля версий
bsl-analyzer-app diagnostics baseline check -s .
```

Для раздельного набора настройте `directory`; CI-команда остаётся той же и проверяет
все enabled разделы атомарно. При `include` находки остальных владельцев остаются в
обычных контейнерах отчёта как `unsuppressed`: SARIF/JSON/JSONL/JUnit показывают
selection и policy-счётчики, а корневой массив GitLab Code Quality и его diagnostic
fingerprint не меняют форму. Локально можно сузить чтение для разбора расхождения:

```bash
bsl-analyzer-app diagnostics baseline check -s . --partition extension:MyExtension
```

Не используйте выбранный раздел как замену общей проверке в обязательном шлюзе CI.
Миграция существующего файла версии 1 выполняется один раз через
`create --from-v1 <путь>`, после чего в систему контроля версий добавляются
`manifest.json` и активные объекты каталога.

В CI запускайте `check`: команда завершается неуспешно при новых или исчезнувших
замечаниях и не изменяет файл. После ревью намеренного изменения разработчик
обновляет его локально и снова проверяет:

```bash
bsl-analyzer-app diagnostics baseline update -s .
bsl-analyzer-app diagnostics baseline check -s .
```

Инкрементальный и diff-анализ подходят для отчётов, но не доказывают отсутствие
замечаний во всём проекте и потому не допускаются командами изменения базовой линии.
GitLab Code Quality остаётся корневым массивом активных замечаний; сводка базовой
линии добавляется в JSON, JSONL, SARIF и JUnit, но не как фиктивная находка GitLab.

Для rollout добавьте `include` с небольшой группой, выполните полный `create` и
оставьте общий `check` обязательным CI-gate. Для rollback удалите `include` и
восстановите отсутствующие объекты явным полным либо выбранным `create`; dormant
objects не считаются долговременным архивом после topology-changing full update.
