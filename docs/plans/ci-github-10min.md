# CI: pull_request-запуск за 10 минут

## Задача

Владелец просит, чтобы pull_request-запуск `.github/workflows/ci.yml` укладывался
в **10 минут максимум**. Сейчас он идёт 26–28 минут. Проверки разрешено снимать или
оптимизировать, если иначе нельзя; владелец отдельно предложил «увеличить
параллельность, если GitHub это не ограничивает». Менять можно только CI-контур:
`ci.yml`, CI-профили сборки в `Cargo.toml`, вспомогательные CI-скрипты. Платные
larger runners и смена провайдера CI исключены, поведение продукта не меняется.
Для push-запусков (develop, включая release gate «Partitioned baseline 1.6M»)
план называет ожидаемое время и судьбу каждой проверки. Каждое снятие или перенос
проверки называется вместе с ценой. Окончательный набор утверждает владелец.

При согласовании первой редакции владелец расширил область: «вообще я не
против и самой оптимизации тестов, очень жёстко в этом плане заданы рамки».
Поэтому план рассматривает и правку кода тестов: ускорение самых долгих тестов
полного прогона, разгрузку тяжёлых тестов, устранение дублирования (раздел
«Ускорение самих тестов», шаги T1–T6). Границы прежние. Прод-код не меняется:
ускорение, требующее правки прод-файла, — вопрос В1. Поведение продукта не
меняется. Каждый перенос, снятие или переделка проверки называется с ценой.
Окончательный набор утверждает владелец. По правилу владельца 4ae4c791 тест,
зависящий от скорости CPU, таймеров или времени, исправляется без этой
зависимости, а если нельзя — помечается ignore.

Полный текст задания — в задаче pio `ci-github-10min`. Этот документ — контракт
для исполнителя.

## Вне рамок

- Правка прод-кода, в том числе тестовых швов в прод-файлах. Где она нужна для
  ускорения теста, это вопрос В1, а не шаг.
- `release.yml`: на PR и push CI он не влияет.
- Job `corpus-contract` (`ci.yml:314-346`, 1:59) и job
  `partitioned-baseline-scale` (`ci.yml:883-897`) — текст не меняется.
- `[profile.dev.package."*"] opt-level = 3` (`Cargo.toml:165-166`) не меняется.
  Уровень оптимизации воркспейс-крейтов — отдельный вопрос, см. T4 и В2.
  При тёплом кэше зависимости не пересобираются: в эталонном запуске все строки
  `Compiling` относятся к воркспейс-крейтам (см. «Замеры эталонного PR-запуска»).
  Значит, на целевую метрику профиль не влияет.
  Понижение замедлит тесты (оптимизированные зависимости: salsa, rusqlite с
  bundled SQLite) и сломает привычную локальную сборку.
- Фильтры путей (`paths-ignore`). Длительность одного PR они не уменьшают. Кроме
  того, тесты читают файлы документации: build script `ide-diagnostics`
  перечисляет `crates/ide-diagnostics/docs/*` («Found 202 documented codes»
  в выводе cargo). Правка «только документации» может уронить тест, а CI на неё
  тогда не запустится.
- Больше потоков libtest (`--test-threads`) в части A. Замер: `mcp_server` lib
  локально — 320 с при 4 потоках, 126 с при 8, 104 с при 16. При 16 потоках
  упал `state::sync::tests::continuous_events_do_not_reset_rescan_debt_backoff`.
  Причина — гонка на общем тестовом флаге, её чинит T1.1. Потоки CI меняются
  только в T5 и только по В3.
- nextest в CI и сборка один раз с раздачей бинарников по job'ам (archive + `needs`).
  Локально nextest используется только для замеров T-раздела.
  Цепочка сборка → раздача → прогон последовательна: около 4:30 сборки, ~1 мин
  выгрузки и загрузки, затем прогон. Выигрыша против независимых шардов нет,
  а это новый инструмент и новая зависимость.

## Допущения, принятые без вопроса

1. **Лимит параллельных job'ов — 20 (Free).** Аккаунт `itrous` — личный (`type:
   User`), API план не показывает (`plan: null`). Лимит считается по аккаунту,
   владельцу репозитория. Запуски PR из форков (`flamber-dev`, `defin85`)
   выполняются в базовом репозитории и тоже занимают слоты этого аккаунта. На Pro
   (40) оценки только улучшаются.
2. **Обязательных status checks на develop нет.** `gh api
   repos/itrous/bsl-analyzer/branches/develop/protection` возвращает объект без
   ключа `required_status_checks`. Rulesets этот эндпоинт не показывает, они
   проверены отдельно: `gh api repos/itrous/bsl-analyzer/rulesets` → `[]`,
   `gh api repos/itrous/bsl-analyzer/rules/branches/develop` → `[]`. Поэтому id
   и имена job'ов можно менять, не трогая настройки защиты ветки.
3. **Числа берутся из эталонных запусков.** PR-запуск 39331071546 (2026-10-19,
   зелёный, 26:37, `headSha 9785b9e5`) и push-запуск 39293825917 (зелёный, 28:09).
   Логи job'ов скачаны через `gh api repos/itrous/bsl-analyzer/actions/jobs/<id>/logs`.
   Время компиляции внутри шага взято из строк `Finished \`test\` profile … in`.
   Время прогона — по меткам времени строк `Running`.
4. **Цель ≤ 10:00 относится к запуску без очереди.** То есть все job'ы стартуют
   сразу и кэш тёплый. Холодный кэш бывает после смены `Cargo.lock` или
   `Cargo.toml`, при выходе нового stable rustc и в первом запуске нового job'а.
   Это разовые события, их цена названа в шаге 6 и в оценках.
5. **Замер делается через `workflow_dispatch` на ветке задачи, а не через PR.** В
   `ci.yml:8` уже есть `workflow_dispatch`. `gh workflow run ci.yml --ref <ветка>`
   выполняет версию файла из ветки. Обычный push ветки CI не запускает:
   `on.push.branches: [develop]`, `ci.yml:4-5`.
6. **Порог приёмки замера — 9:00 на job.** Минута запаса оставлена под разброс
   раннеров: Check шёл 24:30 в 39331071546 и 19:16 в 39293825917.

## Устройство сейчас

Ссылки на строки даны для базового коммита
`22146bb32cc96189eefa50d332ef8122c6949838`.

### Workflow

1. Триггеры: `push` в develop, `pull_request` в develop, `workflow_dispatch`
   (`ci.yml:3-8`). Общий env: `CARGO_TERM_COLOR: always`, `RUSTFLAGS: "-D
   warnings"` (`ci.yml:10-12`). Блоков `concurrency`, `timeout-minutes` на job,
   `needs`, `strategy.matrix` в файле нет: `grep -nE
   'concurrency|timeout-minutes|needs:|matrix' .github/workflows/ci.yml` находит
   только шаговый `timeout-minutes: 60` в `ci.yml:332`.
2. Job `check` (`ci.yml:15-312`, ubuntu-latest) идёт последовательно: fmt
   (`:22`), `scripts/test-ci-test-output.py` (`:23`), clippy
   (`:24`), `cargo test --locked -p bsl-check-executor` (`:25-26`), затем точечные
   шаги. «Workspace cache paths and scopes» — `:27-38`. «Portable workspace lease
   profiles» — `:39-68`, 14 вызовов `run_exact`. «Drift contract» — `:69-298`, 208
   вызовов `run_exact`. «Vector lifecycle…» — `:299-311`. В конце полный прогон
   `cargo test --all --no-fail-fast` (`:312`).
3. Job `corpus-contract` (`ci.yml:314-346`) делает 6 вызовов `run_nonempty`
   с разными `-p`.
4. Job `windows-mcp` (`ci.yml:348-881`, windows-latest) состоит из 25 шагов
   с cargo. Подряд идут «Build binary» (`:355-356`), «Native check executor
   tests without 1C» (`:357-366`), «Build native check executor» (`:367-368`),
   выгрузка `native-check-windows-amd64` (`:369-372`), затем 22 шага тестов
   (`:373-881`). Внутри шагов — 282 различных кортежа (выбор cargo, фильтр,
   точность). Команда `python3 -I ci_selections.py .github/workflows/ci.yml
   windows | wc -l` даёт `282`; скрипт приведён в приложении A.
5. Job `partitioned-baseline-scale` (`ci.yml:883-897`) выполняется только при
   `github.event_name != 'pull_request'` (`:888`): четыре прогона `cargo test
   --release … -- --ignored`.
6. Обёртки. `run_nonempty` проверяет, что вывод содержит `test result: ok\. [1-9]`
   (например `ci.yml:31-35`). `run_exact` делает **один вызов cargo на одно имя**
   с `--exact` и тем же гейтом (`ci.yml:42-54`, `:74-83`, `:620-629`,
   `:650-662`, `:683-692`). Потоковая обёртка (`>>>`/`<<<`, `tee`) есть только
   в шаге «Shared LSP…» (`ci.yml:511-522`).
7. `scripts/test-ci-test-output.py`. Строки 12-16 находят обёртки
   `run_(exact|nonempty)`. Строки 20-24 требуют, чтобы проверялись все
   определения. Строки 25-36 гоняют на заглушке cargo три случая:
   успех, пусто, падение. Строки 39-70 проверяют, что обёртка шага «Shared LSP
   and MCP owner and editor lifecycle» выводит потоком. Строки 74-85 требуют,
   чтобы в job'е `windows-mcp` не было ни одного `cargo test` вне обёртки.
8. `Cargo.toml:162-166`: `[profile.dev] debug = 0`, `[profile.dev.package."*"]
   opt-level = 3`. `release.yml:75` собирает `bsl-native-check` на тег
   (`--release --target …`).
9. Артефакт `native-check-windows-amd64` в репозитории никто не потребляет.
   `grep -rn 'native-check-windows' --include='*.yml' --include='*.md' --include='*.rs'
   --include='*.py' --include='*.sh' --include='*.ps1' .` находит `ci.yml:371`
   и `docs/mcp/NATIVE_CHECK_EXECUTOR.md:23`. Вторая строка — про файл релиза
   `bsl-native-check-windows-amd64.exe`, его собирает `release.yml`. Тесты не читают `target/debug/bsl-analyzer-app` по
   пути: `grep -rln 'target/debug' crates xtask scripts --include='*.rs'
   --include='*.py' --include='*.sh'` пуст, а интеграционные тесты получают бинарник
   через `CARGO_BIN_EXE_bsl-analyzer-app` (например
   `crates/bsl-analyzer/tests/source_set_cli.rs`, 9 мест).

### Замеры эталонного PR-запуска 39331071546

Job'ы: `Check` — 24:30, `Windows MCP transports + secure broker` — 26:34,
`Corpus contract` — 1:59. Release gate пропущен.

**Linux `check`.** Подготовка: setup 0:01, checkout 0:02, toolchain 0:11, rust-cache 0:15.
Clippy — 1:08, точечные шаги — 6:39 (0:09 + 2:44 + 0:14 + 3:26 + 0:06). `cargo
test --all` — 16:04: компиляция `Finished … in 3m 28s` (лог job'а 118329090841,
22:06:11Z) и прогон 754 с. Прогон по классам целей, по меткам `Running` в логе:

| класс | секунд | главные бинарники |
|---|---|---|
| lib + bin unit-тесты (50 бинарников) | 264 | `mcp_server` lib — 209 (1722 теста), `ide_diagnostics` — 14, `bsl_analyzer_app` — 11 |
| интеграционные `tests/[a-m]*` (170 целей) | 180 | `method_increment_churn` — 21, `http` — 10, `line_tokens` — 9.5 |
| интеграционные `tests/[n-r]*` (55) | 117 | `retired_material` — 48, `reparse` — 23, `references` — 13 |
| интеграционные `tests/[!a-r]*` (69) | 179 | `source_set_cli` — 50, `shared_workspace_backpressure` — 43, `shared_workspace_backend` — 29 |
| doc-тесты | 7 | — |

Суммы `tests/` по первой букве имени цели, в секундах:
a 9.8, b 8.9, c 23.9, d 25.0, e 4.3, f 0.8, g 1.4, h 17.5, i 38.4, l 17.8, m 32.2,
n 7.3, o 2.4, p 22.1, q 4.0, r 88.4, s 171.4 (из них `sh*` 85.8, `so*` 51.2,
`se*` 26.3), t 3.5, u 1.5, v 2.7, w 0.1. Числа целей по маскам дают пробы
`cargo test --workspace --test '<маска>' --no-run | grep -c 'Executable tests/'`:
`[a-m]*` — 170, `[n-r]*` — 55, `[!a-r]*` — 69, `--tests` — 294.

Внешних крейтов среди строк `Compiling` нет ни на одной ОС: на Linux 150 строк,
на Windows 195, все — члены воркспейса (`crates/*`, `xtask`). Зависимости
приходят из rust-cache.

Точечные шаги Linux дублируют полный прогон: те же тесты, без `--ignored`, их
гейт требует ≥1 passed. Отличаются они только сборкой под `-p <пакет>`, то есть
с другой унификацией фич (п. «Пробы», 1). Хеш бинарника `mcp_server` в точечных
шагах — `bd3183ff…`, в полном прогоне — `a76832a6…` (строки 710 и 14119 лога
job'а 118329090841).

**Windows `windows-mcp`.** Подготовка: setup 0:02, checkout 0:07, toolchain 0:21,
rust-cache 0:34, итого 1:04. Компиляция по строкам `Finished` — 1024 с из 26:34.
За один прогон воркспейс-крейты собраны 195 раз (45 крейтов): `stdx`, `intern`,
`bsl-platform`, `bsl-metadata` — по 9 раз, `mcp-server` — 8, `vfs`, `syntax`,
`parser`, `project-model`, `base-db` — по 7. Причина — разные наборы `-p` в шагах,
см. «Пробы», 1. Крупные компиляции по вызовам:

| вызов | компиляция, с |
|---|---|
| Build binary `cargo build -p bsl-analyzer --bin bsl-analyzer-app` | 125.0 |
| Build native check executor (release+LTO) | 86.0 |
| `-p ide-diagnostics --lib …` (первый в шаге :373) | 59.2 |
| `-p bsl-analyzer --test diagnostics_config_cli` | 42.3 |
| `-p mcp-server --test diagnostics_baseline` (первая сборка под `-p mcp-server`) | 78.0 |
| `-p mcp-server --lib workspace_cache_` (lib-харнесс) | 61.0 |
| `-p bsl-search --lib lifecycle` | 45.6 |
| `-p bsl-analyzer diagnostics_baseline_create` (все цели пакета) | 50.1 |
| `-p ide selective_loader_…` (все цели `ide`) | 130.0 |
| `-p ide-host-core --lib workspace::` | 46.7 |
| `-p mcp-server workspace_lease::tests::observed_foreign_owner_is_permanent` (все цели) | 54.1 |

Каждый вызов `run_exact` стоит на Windows `Finished` около 0.67 с плюс запуск
процесса. «Drift contract» — 179 вызовов: 294.7 с, из них 120.3 с на проверку
свежести cargo и 145 с собственного времени тестов. Время тестов почти целиком
забирают три теста: 35.3 с, 32.7 с, 17.6 с. На Linux «Drift contract» — 208
вызовов: 205.6 с, из них 72.2 с cargo и 117 с тестов.

Самые долгие прогоны тестов на Windows: `--test shared_workspace_backpressure` —
48.6 с, `--test shared_workspace_backend` — 39.9 с, `-p mcp-server --lib
state::bootstrap::tests::a_workspace_without_a_watch_follows_edits_through_the_poll
--exact` — 31.0 с, `--bin bsl-analyzer-app cli::logging` — 22.2 с.

**Release gate на push** (39293825917, job 118201074910): 13:02. Четыре шага по
2:27, 1:21, 7:24 и 1:20, из них компиляция 135, 68, 431 и 70 с. Сами тесты идут
11–12 с. В других push-запусках: 14:40 (39285066819), 14:51 (39257124741).

**Зависания.** Из 60 последних запусков `ci.yml` (`gh run list --workflow ci.yml
--limit 60`) около 20 длились 6:00–7:42. Висли шаг `cargo test --all` в Check и
шаг «Shared LSP…» в Windows. Сработал таймаут job'а GitHub по умолчанию, 6 ч.

### Пробы, сделанные при составлении плана

Локально, `cargo 1.97.1`. Целиком собрано `cargo test --workspace --no-run`:
3:55, 3607 с user.

1. **Узкий `-p` после сборки `--workspace` пересобирает дерево.** После `cargo
   test --workspace --no-run` команда `cargo test <выбор> --no-run` собрала
   крейтов: `-p ide-diagnostics --lib` — 93, `-p mcp-server --lib` — 60,
   `-p bsl-analyzer --test diagnostics_config_cli` — 67, `-p bsl-search --lib` —
   103 (177 с), `-p project-model` — 27, `-p ide-host-core --lib` — 25. Смена
   **цели** внутри одного `-p` пересборки не вызывает. После `-p mcp-server`
   (1 крейт, 33.5 с) `-p mcp-server --test http` и `-p mcp-server --lib` собрали
   0 крейтов, после `-p bsl-analyzer` `--bin`, `--lib` и `--test source_set_cli`
   тоже 0. Повторный `cargo test --workspace --no-run` собрал 0: варианты
   соседствуют.
   `CARGO_RESOLVER_FEATURE_UNIFICATION=workspace` отклонён: «ignoring
   `resolver.feature-unification` without `-Zfeature-unification`».
2. **libtest принимает несколько фильтров.**
   `cargo test --workspace --lib -- --exact <имя1> <имя2> no_such::test` прошёл
   с кодом 0. В `mcp_server` выполнено `2 passed`, несуществующее имя тихо
   ничего не выбрало. Отсюда поимённый гейт по строкам `test <имя> ... ok`.
3. **Формат вывода libtest** (`rustc --test`, файл-проба). Тест с `#[should_panic]`
   печатается как `test m::panics - should panic ... ok`, игнорируемый — как
   `test m::skipped ... ignored`. `--list` печатает и игнорируемые
   (`m::skipped: test`), `--list --ignored` — только их.
4. **Маски `--test` работают** (см. числа выше). Маска, которой не соответствует
   ни одна цель (`'[!a-z]*'`), — ошибка cargo со списком целей.
5. **Примеры.** Обычный `cargo test --workspace` собирает 10 примеров в
   не-тестовом режиме (`profile.test=false` в `--message-format=json`).
   `cargo test --workspace --examples` собирает их с `--test` (`-v`: `--crate-name
   search_demo --test`).
6. **Разбиение unit-тестов фильтром и `--skip`.** Для `cargo test --workspace --lib
   --bins -- --list` всего 9450. С фильтрами `state::overlay_ state::bootstrap::
   state::embed:: state::sync::` — 235, все в `mcp_server`. С `--skip` по тем же
   строкам — 9215. 235 + 9215 = 9450. Локально `--report-time` при 4 потоках
   (`RUSTC_BOOTSTRAP=1 …/mcp_server-* --test-threads=4 -Z unstable-options
   --report-time`): сумма времени тестов 1271 с, из них под фильтрами 727 с
   (overlay_backlog 193, overlay_retry 92, bootstrap 164, embed 157, sync 121).
   Прогон шёл 320 с при 42 % CPU одного ядра, тесты ждут, а не считают.
7. **Обёртки из шага 1** проверены на реальном cargo и на отрицательных
   контролях:
   - `run_tests --exact -p mcp-server --lib -- <2 имени>` — код 0;
   - тот же вызов с `no_such::test` — код 1, `::error::no_such::test ran no test`;
   - `run_tests -p mcp-server --test http -- no_such_filter_xyz` — код 1;
   - `list_tests --exact bsl_search … -- workspace_lease::tests::…` (чужой бинарник) —
     код 1;
   - `list_tests --exact partitioned_baseline_scale -p ide --test
     partitioned_baseline_scale -- loads_and_classifies_1_6m_entries_with_bounded_rss`
     (тест `#[ignore]`, `crates/ide/tests/partitioned_baseline_scale.rs:55`) — код 1,
     `::error::partitioned_baseline_scale has no runnable …`. Имя при этом стоит
     в выводе `--list` (строка `…: test` найдена 1 раз), то есть отказ дало
     вычитание игнорируемых;
   - `list_tests source_set_cli …`, `list_tests bsl_analyzer_app … -- workspace_cache_scope_
     cli::logging`, `list_tests vector_journal_cli --workspace --test vector_journal_cli` —
     код 0.

### Внешние факты GitHub и Swatinem/rust-cache

- Лимиты (docs.github.com/en/actions/reference/limits): одновременно идут не
  больше 20 job'ов на Free, 40 на Pro, 60 на Team; matrix — до 256 job'ов; кэш —
  10 GB на репозиторий.
- Раннеры для публичного репозитория (`visibility: PUBLIC`): `ubuntu-latest`
  и `windows-latest` — по 4 CPU и 16 GB.
- Кэш сейчас: `gh api repos/itrous/bsl-analyzer/actions/cache/usage` — 11 кэшей,
  11.58 GB. Кэш `check` на develop — 1133 MB, `windows-mcp` — 1222 MB,
  `corpus-contract` — 660 MB, `partitioned-baseline-scale` — 612 MB. PR-кэши по
  ~1.2 GB.
- `rust-cache@v2` (`src/config.ts`) строит ключ так:
  `prefix-key` + (`-shared-key` или `-GITHUB_JOB`) + `-OS-arch` + `-envhash`, затем
  `-lockhash`. Значения matrix в ключ **не входят**. Restore-ключ — без
  `lockhash`. В `envhash` входит версия rustc. Значит, все ноги matrix одного job'а
  делят ключ. Перед сохранением кэш чистится по **именам** пакетов из метаданных
  (`cleanup.ts`, `keepPkg`), а не по использованию в этом запуске. При точном
  попадании в ключ кэш не сохраняется.

## Решение

Каждый job получает свой короткий непрерывный кусок работы. Все job'ы PR
стартуют одновременно, без `needs`. Время PR равно самому долгому job'у.

- **Linux.** Job `lint`: fmt, скрипт проверки CI, clippy. Job `check` становится
  matrix из 5 ног. Каждая нога гоняет свою часть `cargo test --workspace` при
  **той же** унификации фич, что и нынешний `cargo test --all`. Части задаются
  взаимно дополняющими фильтрами unit-тестов и масками интеграционных целей.
  Точечные шаги Linux больше не выполняют тесты. Их гейт «имя существует и не
  игнорируется» проверяет `--list` на уже собранных бинарниках.
- **Windows.** Job `windows-mcp` делится на 4 job'а по наборам `-p`, чтобы
  внутри job'а одно дерево собиралось один раз. Серии `run_exact` и вызовы с
  одинаковым выбором cargo сливаются в один вызов с многими фильтрами
  и поимённым гейтом. Набор кортежей (выбор, фильтр, точность) не меняется.
  «Build binary» и сборка native-исполнителя с артефактом уходят в
  push-only job.
- **Очередь.** Добавляются `concurrency` с отменой устаревших PR-запусков
  и `timeout-minutes: 60` у изменённых job'ов.
- **Тесты (часть B, T1–T6).** Гонки и окна времени в тестах убираются.
  Ожидания таймеров продукта укорачиваются швом (по В1). 203 интеграционных
  файла сливаются в 13 целей. CPU-тяжёлые тесты разгружаются (по В2). Затем
  ноги `check` сокращаются с 5 до 3, если это позволяет замер R4.

### Оценка времени PR-запуска (тёплый кэш)

Подготовка Linux — 0:29 (1+2+11+15 с), Windows — 1:04 (2+7+21+34 с).

**Компиляция Linux** — 208 с на ногу, это верхняя граница: столько собирался
весь `cargo test --workspace` с тёплыми зависимостями. Каждая нога собирает
подмножество.

**Прогон `mcp_server` lib** на CI — 209 с. Он делится пропорционально локальным
суммам: под фильтрами 727/1271 = 57 %, то есть 120 с, остаток — 89 с. Остальные
unit-бинарники — 55 с (264 − 209).

**Компиляция Windows** — сумма строк `Finished` эталонного job'а для деревьев,
которые job собирает. Она завышена: часть общих крейтов в эталоне уже была
собрана раньше.

Прогон тестов в слитых вызовах оценён как max(самый долгий тест; сумма/4) плюс
0.7 с на вызов.

| job | подготовка | компиляция | прогон и гейты | итого | ×1.25 |
|---|---|---|---|---|---|
| `lint` | 0:29 | clippy 1:08 | fmt 0:07, скрипт 0:01 | 1:45 | 2:11 |
| `check (unit-a)` | 0:29 | ≤3:28 | 89+55 с, гейты ~20 с | 6:41 | 8:21 |
| `check (unit-b)` | 0:29 | ≤3:28 | 120 с + 5 с пустых бинарников + doc 7 с | 6:09 | 7:41 |
| `check (tests-a-m)` | 0:29 | ≤3:28 | 180 с | 6:57 | 8:41 |
| `check (tests-n-r)` | 0:29 | ≤3:28 | 117 с + примеры ~30 с | 6:24 | 8:00 |
| `check (tests-rest)` | 0:29 | ≤3:28 | 179 с, гейты ~5 с | 7:01 | 8:46 |
| `corpus-contract` (без изменений) | — | — | — | 1:59 | 2:29 |
| `windows-mcp-contract` | 1:04 | 139 + 46 с | ~55 + 25 с | 5:34 | 6:58 |
| `windows-mcp` | 1:04 | 139 + 80 с | ~53 с | 5:36 | 7:00 |
| `windows-analyzer` | 1:04 | ~235 с | ~135 с | 7:14 | 9:03 |
| `windows-core` | 1:04 | 12 + 59 + 15 + 130 + 47 с | ~7 с | 5:34 | 6:58 |

**Оценка PR-запуска — около 7:15, худший случай около 9:05.** Это не выше
10:00. Пока на Windows не измерено, оценка условна: шаг 6 меряет её до того, как
результат считается достигнутым.

### Экономия по направлениям разведки

1. Дубли точечных шагов Linux убираются, гейт остаётся на `--list`.
   Освобождается 6:39 с пути Linux — время шагов `ci.yml:25-311` в эталоне.
2. Серии `run_exact` сжаты. Windows: 179 + 16 + 14 + 2 вызова превращаются в 2.
   Это ~5:45 последовательного времени («Drift» 4:54, «WL» 0:30, «Portable» 0:21)
   против оценки ~1:00. Linux: 222 вызова (3:40 в шагах `ci.yml:39-298`)
   заменены семью вызовами `list_tests` (пять в `unit-a`, два в `tests-rest`).
   Каждый делает два вызова cargo `--list`, итого 14 вызовов по ~1–3 с:
   ~20 с в `unit-a` и ~5 с в `tests-rest`.
3. «Build binary» и native-сборка вынесены с пути PR: −2:06 и −1:28.
4. Шардинг: Linux 24:30 превращается в ≤7:01 на ногу, Windows 26:34 — в ≤7:14
   на job. Это главный выигрыш.
5. `opt-level` — отказ, см. «Вне рамок» и замер T4.
6. `concurrency` и `timeout-minutes` латентность одного PR не меняют. Они
   освобождают слоты: устаревшие запуски отменяются, зависший job держит слот
   ≤60 мин, а не 6 ч.

### Push-запуск в develop

Job'ы PR, плюс `partitioned-baseline-scale` без изменений, плюс push-only
`windows-build`. Release gate не трогается и остаётся на push. Он и задаёт время
push: **13–15 минут**, по наблюдениям 13:02, 14:40, 14:51; job'ы пути PR
укладываются в ≤ 7:15. `windows-build`: подготовка 1:04 + 125 с + 86 с + выгрузка
— около 4:40 при тёплом кэше.

### Параллельность и лимиты GitHub

На PR идёт 11 job'ов: `lint`, 5 ног `check`, `corpus-contract`, 4 Windows. На
push — 13: ещё release gate и `windows-build`. Раньше было 3 и 4.

На Free (лимит 20) без очереди помещается один запуск. Один PR и один push
одновременно — 24 job'а, четыре ждут. Два PR одновременно — 22 job'а, два ждут. Ждущий job
стартует, когда освободится первый слот. Раньше всех кончаются `lint` и
`corpus-contract` (~2:00), так что опоздавший запуск займёт около 9–10 мин. При
трёх одновременных PR (33 job'а) 13 ждут вторую волну, и последний запуск идёт
до ~14 мин. Гарантия ≤10:00 держится, пока одновременно идёт не больше двух
запусков на Free и не больше трёх на Pro (лимит 40).

Отмена устаревших PR-запусков (`cancel-in-progress`) возвращает слоты сразу после
нового push в PR. `timeout-minutes: 60` не даёт зависанию держать 11 слотов по
6 ч.

### Кэш

- Ноги `check` делят ключ job'а `check`, это нынешний develop-кэш на 1133 MB. С
  первого же запуска они тёплые. Сохраняет только нога `unit-a` (`save-if`):
  она собирает lib-тесты всех 45 пакетов, а значит, все dev-зависимости. Её
  набор зависимостей покрывает остальные ноги.
- `lint` получает свой ключ `lint`, его метаданные check-режима с тестовой
  сборкой не пересекаются. Job `windows-mcp` сохраняет id и ключ, его кэш тёплый.
  У `windows-mcp-contract`, `windows-analyzer`, `windows-core`, `windows-build` и
  `lint` ключи новые: первый запуск каждого холодный.
- Оценка размера на develop: check ~1.1 GB, lint ~0.4, corpus 0.66, release 0.61,
  четыре Windows-кэша ~0.8–1.2 каждый, `windows-build` ~0.5. Итого ~6.5–7 GB из
  10 GB.

## Ускорение самих тестов

Раздел добавлен по замечанию владельца: «вообще я не против и самой оптимизации
тестов». Ниже разобрано, на что уходит время полного прогона, и предложены правки
кода тестов T1–T6. Прод-код они не меняют. Ускорение, для которого нужна правка
прод-файла, вынесено в вопрос В1. По правилу владельца 4ae4c791 тест, зависящий
от скорости CPU, таймеров или времени, исправляется без этой зависимости, а если
это невозможно — помечается ignore.

Часть A (шаги 1–7) самодостаточна: цель ≤ 10:00 по оценке достигается и без
T-шагов. T-шаги дают запас, сокращают число job'ов PR (T6) и ускоряют локальный
`cargo test --workspace` и pre-commit хук.

### Замеры тестов

Прогон `cargo test --all` в эталоне длится 754 с, по меткам `Running` лога
118329090841:

| бинарник | с на CI | тестов | на что уходит время |
|---|---|---|---|
| `mcp_server` lib | 209 | 1722 | ожидание таймеров продукта (классы W1–W3) и общая блокировка (S) |
| 238 интеграционных бинарников короче 1 с | 106 | — | запуск процесса и инициализация, ~0.44 с на бинарник (класс B) |
| `source_set_cli` | 50 | 38 | процессы CLI и MCP; `…_s12_proxy_…` ждёт 30 с (W4) |
| `retired_material` | 48 | 5 | хэши 8.5 МБ файлов по 298 длинам окна в `opt-level 0` (C) |
| `shared_workspace_backpressure` | 43 | 2 | 17 МБ правок через LSP-процесс |
| `shared_workspace_backend` | 29 | 10 | процессы LSP и MCP |
| `reparse` | 23 | 8 | случайные правки, CPU (C) |
| `method_increment_churn` | 21 | 9 | CPU (C) |
| `bsl_analyzer_app` bin | 11 | 220 | `cli::check_config::tests::check_config_accepts_*` — 2 × 10.9 с (W2) |

Собственное время каждого теста измерено локально:
`cargo nextest run --locked --workspace --no-fail-fast --test-threads 8`.
nextest 0.9.146 уже стоит на машине, в CI он не вводится. Каждый тест идёт
отдельным процессом, поэтому число не включает ожидание общих блокировок.
Итог: 11 889 тестов, сумма 2859 с. У 317 тестов дольше 1 с — 1178 с, у 68
тестов дольше 5 с — 668 с.

Сборка тоже меряется локально, с `CARGO_INCREMENTAL=0`, как в rust-cache.
`cargo test --workspace --no-run -j 4` при тёплых зависимостях: 125 с, 619
CPU-с. На CI та же сборка идёт 208 с, отсюда коэффициент CI/локально ≈ 1.66.

### Классы причин

| класс | признак | тестов | собств. время | как устранить |
|---|---|---|---|---|
| W1 | тест держит `BEGIN IMMEDIATE`, продукт ждёт `DEFAULT_BUSY_TIMEOUT` = 30 с (`crates/bsl-search/src/store.rs:156`) | 5: `state::overlay_backlog::tests::{a_refused_commit_is_backed_off_and_returned_to_on_the_system_clock, a_revived_owner_starts_its_busy_schedule_over, an_exhausted_owner_is_revived_by_a_fresh_fact_only, wakes_do_not_step_around_the_backoff, a_busy_commit_is_offered_again_soon}` | 155 с, по 30.2–33.2 с | В1 |
| W2 | адрес без сервера (`postgres://127.0.0.1:1`, `localhost:5432`), пул r2d2 ждёт `connection_timeout` = 5 с (`crates/bsl-search/src/external_baseline/postgres.rs:115`) | кластер 5.0–5.4 с — 29 тестов, 10.0–10.4 с — 4. Большинство — `baseline::tests::*`, `tools::search::hybrid::tests::*`, `external_baseline::postgres::tests::*`, `check_config_accepts_*`. Остальные пятисекундные: `change_hub::tests::dropping_a_hub_whose_thread_cannot_answer_returns`, `bsl-debug session::tests::locals_without_an_answer_is_an_error_not_an_empty_frame` | 188 с | В1 |
| W3 | тест спит интервал продукта: `std::thread::sleep(VERDICT_TTL)` (`crates/mcp-server/src/workspace_lease.rs:2659`); `VERDICT_TTL` и `LOCK_WAIT` = 2 с (`:79`, `:83`), `DRIFT_CHECK_INTERVAL` = 2 с (`crates/mcp-server/src/graph/state.rs:26`) | кластер 2.0–2.3 с — 51, из них в `mcp_server` lib 36 (`workspace_lease::tests` 12, `graph::state` 10). Остальные 15 — процессные тесты `bsl-analyzer` и `ide-diagnostics`, их причина устанавливается в T2.1 | ~107 с | В1 |
| W4 | брокер не поднимается, продукт ждёт бюджет подключения 30 с. Вывод теста: «broker backend did not become reachable within 30s: connect budget expired» | 1: `workspace_cache_scope_s12_proxy_parent_child_composition_mismatch` | 30 с | В1 |
| R | исход зависит от нагрузки, планировщика или окна времени | см. T1 | — | T1, только тесты |
| S | тесты под одной блокировкой процесса `env_lock()` (`crates/mcp-server/src/state/test_support.rs:22`) идут строго по очереди | 107 тестов в `state::{bootstrap,overlay_retry,sync,embed}` и др. | цепочка 100 с, нижняя граница ноги `unit-b` | побочно от W2 и W3 |
| B | отдельный бинарник на каждый файл `tests/*.rs`: своя линковка (~62 МБ на цель `ide`) и свой запуск процесса | 294 цели | см. T3 | T3, только тесты |
| C | CPU-счёт в `opt-level 0` воркспейс-крейтов | `retired_material`, `reparse`, `method_increment_churn`, `line_tokens` | ~100 с на CI | T4 |

Классы W не делают тест нестабильным: при сбое продукта они падают, на медленной
машине только удлиняются. Но они ждут таймер, а правило 4ae4c791 запрещает
завязку теста на время. Поэтому они подпадают под правило.

Дублирование. Дорогое дублирование лежит не в телах тестов. Оно в 294 отдельных
сборках и запусках (B) и в повторном прогоне точечных шагов CI, который убирает
часть A. Совпадающих имён тестов в разных бинарниках 37, все тесты в них идут
миллисекунды (например, `test_preprocessor_if` в `ide`, `ide-diagnostics`,
`lexer`). Это разные слои с одинаковыми именами, а не копии, и удалять их
план не предлагает. В слитых вызовах Windows (шаг 3) тест, попавший под
несколько фильтров одного вызова, libtest выполняет один раз.

### Предложения T1–T6

| шаг | что | правит | выигрыш на CI | цена |
|---|---|---|---|---|
| T1 | Гонки и окна времени (класс R): исправить в коде тестов или перевести в timing-contract профиль | тесты | стабильность. Условие для T5 | утверждения, переведённые в timing-contract, в обычном прогоне не проверяются. Перечень даёт отчёт T1 |
| T2 | Ожидания таймеров продукта (W1–W4) | по В1: прод-файлы (шов) или тесты (ignore) | `mcp_server` lib 209 → ~110 с; `bsl_analyzer_app` 11 → ~1 с; `source_set_cli` 50 → ~25 с; цепочка S 100 → ~60 с | при В1-а — правка прод-файлов, поведение продукта по умолчанию прежнее. При В1-б пути тайм-аута и отказа не проверяются в обычном прогоне, 30 из таких тестов названы в гейтах CI |
| T3 | Слить интеграционные цели без глобального состояния процесса в одну цель на крейт | тесты, `Cargo.toml` крейтов не меняется | сборка воркспейса 208 → ≤120 с, прогон −~80 с на Linux; Windows `windows-core` −~90 с | имена тестов получают префикс модуля; `cargo test --test <файл>` для слитых файлов заменяется на `--test <крейт>_suite -- <файл>::`; файлы одной цели больше не изолированы процессом |
| T4 | `retired_material` — в отдельный крейт с `opt-level 3` | новый член воркспейса, профиль в `Cargo.toml`, `LICENSING.md` | 48 → ~5 с в его ноге | новый крейт и строка профиля; путь гейта чистоты материала меняется в `LICENSING.md`. `opt-level 1` для всего воркспейса отклонён замером: сборка +462 с локально |
| T5 | Больше потоков libtest для `--lib --bins` | `ci.yml` | `mcp_server` lib ~2.5× при 8 потоках (локально 320 → 126 с) | на 4 vCPU тесты идут с перегрузкой; нестабильный тест по правилу 4ae4c791 чинится или игнорируется. Только по В3 |
| T6 | Сократить ноги `check` после T1–T4 | `ci.yml` | 11 → 9 job'ов на PR | каждая нога длиннее; правило — шаг T6 |

Без вопросов владельцу делаются T1, T3 и T6, они правят только тесты и CI. T2,
T4 и T5 выполняются по ответам на В1–В3. Ожидаемый полный прогон Linux одним
процессом после T1–T4 при В1-а: сборка ~120 с, прогон ~500 с (754 − 99 − 80 − 25
− 10 − 43), то есть около 10–11 мин против 16. Это оценка, мерит её T6.

## Цена каждого переноса и снятия

| что | куда | какая защита теряется |
|---|---|---|
| Выполнение 222 точечных тестов Linux (`ci.yml:39-298`) и 7 фильтров (`:27-38`, `:299-311`) под `-p <пакет>` | Те же тесты выполняются в ногах `check` под `--workspace`; гейт имени — `list_tests` | На Linux эти тесты больше не идут под унификацией фич одного пакета (`-p mcp-server`, `-p bsl-search`, `-p bsl-analyzer`). Пример различия: под `-p mcp-server` пакет `profile` собирается без фичи `jemalloc`, а под `--workspace` — с ней. Её включает только `bsl-analyzer`, `crates/bsl-analyzer/Cargo.toml:86`. На Windows переносимое подмножество по-прежнему идёт под `-p` |
| `cargo test --locked -p bsl-check-executor` на Linux (`ci.yml:25-26`) | Все тесты пакета идут в ногах `check`; `--locked` есть у каждой команды ног | Ничего |
| Порядок изоляции: в старых шагах каждое имя или фильтр — свой процесс | Слитые вызовы гоняют тесты одного бинарника параллельно (4 потока libtest) | Если на Windows проявится взаимное влияние тестов, такой тест чинится в той же ветке (правило 11). На Linux те же тесты и сейчас идут параллельно в `cargo test --all` |
| «Build binary» Windows (`ci.yml:355-356`) | push-only job `windows-build` | На PR больше не проверяется сборка `bsl-analyzer-app` на Windows без dev-зависимостей. Сборка с тестовыми фичами остаётся: её делают `--test diagnostics_config_cli` и другие цели через `CARGO_BIN_EXE`. Поломка без dev-фич найдётся после влития в develop |
| «Build native check executor» и выгрузка `native-check-windows-amd64` (`ci.yml:367-372`) | push-only job `windows-build`; release.yml собирает исполнитель на тег (`release.yml:75`) | На PR не проверяется release+LTO-сборка `bsl-native-check` на Windows, и у PR нет этого артефакта (потребителей у него нет, «Устройство сейчас», п. 9). Отладочная сборка и тесты пакета на Windows остаются в `windows-core` |
| Примеры в ноге `tests-n-r` собираются через `--examples` | — | Примеры собираются в тестовом режиме (`--test`), а не обычной сборкой. Ошибка, которая видна только без `cfg(test)`, на Linux не поймается. Clippy `--all-targets` проверяет их по-прежнему |
| T1: верхние границы, окна отсутствия события и паузы, расставляющие гонку, в коде тестов | Ожидание события; если события нет — под `cfg!(timing_contract)` или `ignore = "timing contract: …"` | Переведённое под timing-contract в обычном прогоне и в CI не проверяется: его гоняют вручную (`DEVELOPMENT_RULES.md`, «Тесты и время»). Пример: упор писателя в `shared_workspace_backpressure.rs:433`. Перечень даёт отчёт T1 |
| T2, В1-а: ожидание константы продукта (30 с, 5 с, 2 с) заменено коротким значением через шов | Тот же тест с ожиданием ≤ 50 мс | Тест больше не проверяет, что сработала именно боевая константа: её значение проверяется только чтением кода. Ход по путям тайм-аута сохраняется |
| T2, В1-б: тесты класса W без шва | `ignore` под timing-contract | Пути тайм-аута, отказа подключения и истёкшего TTL не проверяются в обычном прогоне. Поимённые в CI тесты остаются медленными |
| T3: 203 файла `tests/*.rs` — в 13 целей `*_suite` | Те же тесты в одной цели на крейт | Файлы одной цели больше не изолированы процессом: скрытое общее состояние (`OnceLock`, кэш) проявится как взаимное влияние, его ловит стресс-проба T1.3. `cargo test --test <файл>` для перенесённых файлов не работает. Ошибка компиляции одного файла роняет всю цель крейта |

Решения владельца на согласовании: оставить «Build binary» на PR (+2:06
к `windows-mcp-contract`, итого ~7:40) и/или native-сборку (+1:28 к
`windows-core`, итого ~7:00). Оба варианта по оценке укладываются в 10:00.

## Шаги

Все правки делаются в worktree задачи. Ветка `pio/ci-github-10min`, база —
`22146bb3`. Порядок: шаги 1–7 (часть A), затем T1–T6 (часть B), затем шаг 8. Обёртки во всех шагах ниже — дословно код из шага 1.

### Шаг 1. Обёртки `run_tests` и `list_tests`

Обе обёртки вставляются в начало `run:` каждого bash-шага, где вызываются. Отступ
определения — 10 пробелов, как у нынешних обёрток. `run_exact` из файла
исчезает. `run_nonempty` остаётся только в `corpus-contract` (`ci.yml:336`): этот
job не меняется.

```bash
          run_tests() {
            # run_tests [--exact] <cargo test selection>... [-- <filter>...]
            # One cargo invocation for every filter. A filter that ran no passing test
            # fails the step, so a renamed or moved test cannot silently drop out; with
            # no filter the selection must run at least one test. Output streams, so a
            # step the runner cancels still shows the command that hung.
            local exact=() selection=() filters=() log status passed filter missing=0
            if [ "${1-}" = --exact ]; then exact=(--exact); shift; fi
            while [ "$#" -gt 0 ] && [ "$1" != -- ]; do selection+=("$1"); shift; done
            if [ "$#" -gt 0 ]; then shift; filters=("$@"); fi
            log=$(mktemp)
            printf '>>> cargo test %s\n' "${selection[*]} -- ${exact[*]} ${filters[*]}"
            cargo test "${selection[@]}" -- "${exact[@]}" "${filters[@]}" 2>&1 | tee "$log" && status=0 || status=$?
            printf '<<< cargo test %s: exit %s\n' "${selection[*]}" "$status"
            if [ "$status" -ne 0 ]; then
              return "$status"
            fi
            passed=$(sed -n 's/^test \(.*\) \.\.\. ok$/\1/p' "$log" | sed 's/ - should panic$//')
            if [ "${#filters[@]}" -eq 0 ] && [ -z "$passed" ]; then
              echo "::error::cargo test ${selection[*]} ran no test"
              return 1
            fi
            for filter in "${filters[@]}"; do
              if [ "${#exact[@]}" -gt 0 ]; then
                grep -Fxq -- "$filter" <<<"$passed" || { echo "::error::$filter ran no test"; missing=1; }
              else
                grep -Fq -- "$filter" <<<"$passed" || { echo "::error::$filter ran no test"; missing=1; }
              fi
            done
            return "$missing"
          }
```

```bash
          list_tests() {
            # list_tests [--exact] <test binary> <cargo test selection>... [-- <filter>...]
            # Lists without running: every filter must name a listed, non-ignored test of
            # the binary (`mcp_server`, `source_set_cli`, ...); with no filter the binary
            # must list one. The selection is the one the leg already built, so nothing
            # compiles.
            local exact=() binary selection=() filters=() listed ignored runnable status filter missing=0
            local pick='/ Running /{b=$0; sub(/.*deps\//,"",b); sub(/-[0-9a-f]+\)$/,"",b); cur=b; next} cur==want && /: test$/{sub(/: test$/,""); print}'
            if [ "${1-}" = --exact ]; then exact=(--exact); shift; fi
            binary=$1; shift
            while [ "$#" -gt 0 ] && [ "$1" != -- ]; do selection+=("$1"); shift; done
            if [ "$#" -gt 0 ]; then shift; filters=("$@"); fi
            listed=$(cargo test "${selection[@]}" -- --list "${exact[@]}" "${filters[@]}" 2>&1) && status=0 || status=$?
            printf '%s\n' "$listed"
            if [ "$status" -ne 0 ]; then
              return "$status"
            fi
            ignored=$(cargo test "${selection[@]}" -- --list --ignored "${exact[@]}" "${filters[@]}" 2>&1) && status=0 || status=$?
            if [ "$status" -ne 0 ]; then
              printf '%s\n' "$ignored"
              return "$status"
            fi
            runnable=$(comm -23 <(awk -v want="$binary" "$pick" <<<"$listed" | sort -u) <(awk -v want="$binary" "$pick" <<<"$ignored" | sort -u))
            if [ "${#filters[@]}" -eq 0 ] && [ -z "$runnable" ]; then
              echo "::error::$binary lists no runnable test"
              return 1
            fi
            for filter in "${filters[@]}"; do
              if [ "${#exact[@]}" -gt 0 ]; then
                grep -Fxq -- "$filter" <<<"$runnable" || { echo "::error::$binary has no runnable test $filter"; missing=1; }
              else
                grep -Fq -- "$filter" <<<"$runnable" || { echo "::error::$binary has no runnable test matching $filter"; missing=1; }
              fi
            done
            return "$missing"
          }
```

Длинные списки имён задаются bash-массивом по одному имени на строку. Строки
комментариев внутри массива допустимы. Затем массив передаётся как
`"${имя[@]}"`, а вызов обёртки пишется одной строкой без `\`. Иначе скрипт
приложения A не прочтёт кортежи.

**Исход:** обёртки ведут себя как в пробе 7 из «Устройства сейчас». Это
проверяют случаи скрипта из шага 4.

### Шаг 2. Linux: job `lint` и matrix-job `check`

Шаги `ci.yml:15-312` заменяются двумя job'ами.

```yaml
  lint:
    name: Lint
    runs-on: ubuntu-latest
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: python3 scripts/test-ci-test-output.py
      - run: cargo clippy --all-targets --all-features

  check:
    name: Check (${{ matrix.shard }})
    runs-on: ubuntu-latest
    timeout-minutes: 60
    strategy:
      fail-fast: false
      matrix:
        include:
          - shard: unit-a
            select: --lib --bins -- --skip state::overlay_ --skip state::bootstrap:: --skip state::embed:: --skip state::sync::
          - shard: unit-b
            select: --lib --bins -- state::overlay_ state::bootstrap:: state::embed:: state::sync::
          - shard: tests-a-m
            select: --test '[a-m]*'
          - shard: tests-n-r
            select: --test '[n-r]*' --examples
          - shard: tests-rest
            select: --test '[!a-r]*'
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          # The legs share this job's cache key; the unit-a leg builds every library
          # test, so every dev-dependency, and its cache serves the other legs.
          save-if: ${{ matrix.shard == 'unit-a' }}
      - run: cargo test --locked --workspace --no-fail-fast ${{ matrix.select }}
      - if: matrix.shard == 'unit-b'
        run: cargo test --locked --workspace --no-fail-fast --doc
      - name: Named library and CLI regressions exist
        if: matrix.shard == 'unit-a'
        shell: bash
        run: |
          set -euo pipefail
          <list_tests из шага 1>
          mcp_server_names=(
            <имена>
          )
          bsl_search_names=(
            <имена>
          )
          list_tests --exact mcp_server --workspace --lib -- "${mcp_server_names[@]}"
          list_tests --exact bsl_search --workspace --lib -- "${bsl_search_names[@]}"
          list_tests mcp_server --workspace --lib -- workspace_cache_ vector_lifecycle
          list_tests bsl_search --workspace --lib -- lifecycle
          list_tests bsl_analyzer_app --workspace --bin bsl-analyzer-app -- workspace_cache_scope_ cli::logging
      - name: Named integration regressions exist
        if: matrix.shard == 'tests-rest'
        shell: bash
        run: |
          set -euo pipefail
          <list_tests из шага 1>
          list_tests source_set_cli --workspace --test source_set_cli -- workspace_cache_scope_
          list_tests vector_journal_cli --workspace --test vector_journal_cli
```

Как собрать массивы:

- `mcp_server_names` — сначала имена `ci.yml:55-68` (у `run_exact` один аргумент
  — имя). Затем имена `ci.yml:84-298`, у которых первый аргумент `run_exact` —
  `mcp-server`: берётся второй аргумент. Порядок как в файле, повторы
  выбрасываются. Комментарии `ci.yml:72-73`, `:182-185`, `:254`, `:263`, `:284`
  переносятся в массив на те же места: они объясняют, почему этих имён нет
  на Windows. Итого 211 имён.
- `bsl_search_names` — вторые аргументы строк `run_exact bsl-search …` из
  `ci.yml:84-298`. Итого 11 имён.

Комментарий над массивами (WHY, без ссылок на процесс): каждое имя обязано
оставаться в списке тестов Linux и не быть игнорируемым. Иначе переименованный
или удалённый тест исчез бы из покрытия незаметно. Выполняются эти тесты
в ногах `check`.

**Исход:** в job'е `check` нет ни одного `-p` в командах cargo test, и каждая
команда содержит `--workspace` (инвариант I2). Старый job `check`
(`ci.yml:15-312`) заменён, его fmt, скрипт и clippy теперь в `lint`.
Приложение A в режиме `linux-gates` даёт 229 кортежей до и после правки, `diff`
пуст (I3).

### Шаг 3. Windows: четыре job'а и push-only `windows-build`

Job `windows-mcp` (`ci.yml:348-881`) заменяется пятью job'ами. В каждом сначала
идут `actions/checkout@v7`, `dtolnay/rust-toolchain@stable`,
`Swatinem/rust-cache@v2` без параметров, затем `runs-on: windows-latest` и
`timeout-minutes: 60`. У каждого шага с тестами `shell: bash`, `set -euo
pipefail` и `run_tests` из шага 1.

**`windows-mcp-contract`** — «Windows MCP drift contract».

- Шаг «Drift contract and workspace lease regressions»:
  - массив `mcp_server_names` — вторые аргументы строк `run_exact mcp-server …`
    из `ci.yml:630-645` и `:693-871`, единственные аргументы `run_exact …` из
    `ci.yml:663-676` и имена до ` -- --exact` из `ci.yml:433-434`. Порядок:
    сначала `:433-434`, затем `:630-645`, `:663-676`, `:693-871`, повторы
    выбрасываются, итого 199 имён;
  - массив `bsl_search_names` — вторые аргументы строк `run_exact bsl-search …`
    из `ci.yml:693-871`, итого 11;
  - `run_tests --exact -p mcp-server --lib -- "${mcp_server_names[@]}"`;
  - `run_tests --exact -p bsl-search --lib -- "${bsl_search_names[@]}"`.
- Шаг «Vector lifecycle and fenced search construction»:
  - `run_tests -p bsl-search --lib -- lifecycle`;
  - `run_tests -p bsl-search -- engine::tests::workspace_constructor_publish_fence
    engine::tests::embedding_publish_fence`.

**`windows-mcp`** — id и имя «Windows MCP transports + secure broker» сохраняются.

- Шаг «MCP library lifecycle, workspace cache and editor regressions»:
  `run_tests -p mcp-server --lib -- workspace_cache_ check_executor_windows_tests
  vector_lifecycle saved_path_refresh_ queued_source_writer_ editor_path_hints_
  lru_trim_waits_for_analysis_clone_without_publishing_generation
  shared_backend_sessions_keep_tool_gates_and_native_dispatch_local
  invalid_project_freezes_disk_publications_but_keeps_last_good_resident_readable
  invalid_project_closes_mcp_before_retiring_last_good_backend
  pending_baseline_connects_only_after_start
  shared_workspace_defers_workspace_and_reference_credentials_until_activation
  broker::transport:: broker::daemon::shared_tests::`.
- Шаг «MCP transports, broker and baselines»:
  - `run_tests -p mcp-server --test diagnostics_baseline`;
  - `run_tests -p mcp-server --test http`;
  - `run_tests -p mcp-server --test broker`;
  - `run_tests -p mcp-server --test broker -- superseded_backend_lifecycle`;
  - `run_tests -p mcp-server --test name_dictionary`.
- Шаг «Superseded owner lifecycle across MCP targets»: `run_tests -p mcp-server
  --` и фильтры строк `ci.yml:564-568`, `:579-583`, `:593-599`, `:609`, `:611`,
  `:612`. Фильтр — последний аргумент строки. Итого 20 фильтров в порядке
  файла.

**`windows-analyzer`** — «Windows analyzer CLI + shared LSP».

- Шаг «Analyzer CLI, diagnostics configuration and baselines»:
  - `run_tests -p bsl-analyzer --test diagnostics_config_cli`;
  - `run_tests -p bsl-analyzer --test diagnostic_rules_protocol`;
  - `run_tests -p bsl-analyzer --test source_set_cli -- diagnostics_rules_
    workspace_cache_scope_`;
  - `run_tests -p bsl-analyzer --bin bsl-analyzer-app -- workspace_cache_scope_
    cli::mcp cli::logging cli::lsp cli::mcp::tests::`;
  - `run_tests -p bsl-analyzer -- diagnostics_baseline_create
    diagnostics_baseline_update partitioned_baseline_transaction
    selective_baseline_transaction`.
- Шаг с именем ровно «Shared LSP and MCP owner and editor lifecycle» (его ищет
  скрипт):
  - `run_tests -p bsl-analyzer --test shared_workspace_backend`;
  - `run_tests -p bsl-analyzer --test shared_workspace_backpressure`;
  - `run_tests -p bsl-analyzer --lib -- panic_watch:: shared_admission_
    mem_docs::tests:: diagnostics_keep_wire_version_and_reject_results_from_before_an_edit
    diagnostics_cancelled_by_writer_requeue_only_current_open_version
    g1_origin_guard_`.

**`windows-core`** — «Windows core crates».

- Шаг «Native check executor tests without 1C»: `run_tests --locked -p
  bsl-check-executor`.
- Шаг «Diagnostic occurrences, parameter rewrites and configuration»: `run_tests
  -p ide-diagnostics --lib -- handlers::typo::tests:: single_pass::tests::
  handlers::rewrite_method_parameter::tests:: config::tests::`.
- Шаг «Selective baseline path security»:
  - `run_tests -p project-model -- partitioned_baseline_path_security`;
  - `run_tests -p ide -- selective_loader_rejects_unsafe_enabled_paths_and_links`.
- Шаг «Shared workspace host»: `run_tests -p ide-host-core --lib -- workspace::
  editor_view_builds_configuration_from_owner_inputs_and_refreshes_xml`.

Длина командной строки: 199 имён `mcp_server_names` дают 14 802 символа. Предел
Windows — 32 767. Если список когда-нибудь вырастет за ~30 000 символов, его
делят на два вызова `run_tests` с тем же выбором.

**`windows-build`** — «Windows build artifacts», с `if: github.event_name !=
'pull_request'`. Шаги переносятся дословно из `ci.yml:355-356` («Build binary»)
и `ci.yml:367-372` («Build native check executor» и `actions/upload-artifact@v7`
с прежними `name` и `path`). `cargo test` в job'е нет.

**Исход:** приложение A в режиме `windows` до правки и после даёт одинаковые
282 строки, `diff` пуст (I4). Job `windows-build` на PR пропущен, на push
выполняется (I7).

### Шаг 4. `scripts/test-ci-test-output.py`

Скрипт переписывается под новое устройство, его назначение то же. Проверки:

1. Обёртки ищутся регуляркой `^          (?:run_tests|list_tests|run_nonempty)\(\) \{\n.*?^          \}`
   (re.M | re.S). Число найденных равно числу определений
   `^\s*(?:function\s+)?(?:run|list)_\w+\s*\(\s*\)\s*\{`. Найдено хотя бы по
   одной обёртке `run_tests` и `list_tests`. Блоки `run_nonempty` (сейчас один,
   `corpus-contract`) проверяются прежними тремя случаями: успех, пусто, падение
   (нынешние строки 25-36 скрипта).
2. Каждый блок `run_tests` прогоняется в `bash -c` с `set -euo pipefail`
   и заглушкой `cargo() { …; }`, которая печатает заданный текст и возвращает
   заданный код. Случаи и ожидаемый исход:
   - вывод `test fixture::name ... ok`, код 0, `run_tests --exact -p x --
     fixture::name` → 0;
   - тот же вывод, `run_tests --exact -p x -- fixture::other` → не 0, в stdout
     `::error::fixture::other ran no test`;
   - тот же вывод, `run_tests -p x -- name` → 0, а `run_tests --exact -p x -- name`
     → не 0;
   - вывод `test fixture::name - should panic ... ok`, код 0, `run_tests --exact
     -p x -- fixture::name` → 0;
   - вывод `test fixture::name ... ok`, код 101 → код 101, вывод есть в stdout;
   - вывод `test result: ok. 0 passed;`, код 0, `run_tests -p x` → не 0; вывод
     `test a ... ok`, код 0, `run_tests -p x` → 0;
   - при кодах 0 и 101 строка `>>> cargo test -p x -- …` стоит до вывода, а `<<<
     cargo test -p x: exit <код>` — после, каждая ровно один раз;
   - заглушка печатает `test fixture::name ... ok` и делает `kill -KILL $$` →
     returncode −9, строка вывода в stdout, строка `>>>` есть, строки `<<<` нет.
3. Каждый блок `list_tests` прогоняется с заглушкой. Если среди аргументов нет
   `--list`, она возвращает 99. Иначе печатает две секции, `     Running
   unittests src/lib.rs (target/debug/deps/fixture-0123abcd)` и `… (target/debug/deps/other-4567ef01)`.
   Без `--ignored` в секции `fixture` строки `fixture::kept: test`
   и `fixture::muted: test`, в секции `other` — `other::name: test`. С `--ignored`
   только `fixture::muted: test`. Случаи:
   - `list_tests --exact fixture --workspace --lib -- fixture::kept` → 0;
   - то же с `fixture::muted` (игнорируемый) → не 0;
   - то же с `other::name` (чужой бинарник) → не 0;
   - то же с `fixture::gone` → не 0;
   - `list_tests fixture --workspace --lib -- kept` → 0;
   - `list_tests fixture --workspace --lib` → 0, а `list_tests absent --workspace
     --lib` → не 0;
   - заглушка с кодом 101 → код 101.
   Код 99 ни в одном случае не встречается: обёртка не запускает тесты.
4. Шаг `^      - name: Shared LSP and MCP owner and editor lifecycle\n` есть ровно
   один, и внутри него определена `run_tests`.
5. Для каждого job'а, у которого `runs-on:` начинается с `windows`, после
   вырезания блоков обёрток в тексте job'а не находится `cargo\s+["']?\s*test\b`.
   Таких job'ов не меньше четырёх.
6. Matrix job'а `check`:
   - множество аргументов после `--skip` в `select` ноги `unit-a` равно множеству
     фильтров после `--` в ноге `unit-b`, причём оба непусты;
   - ни один `select` не содержит `-p`;
   - `select` обеих ног `unit-*` начинается с `--lib --bins --`;
   - `--examples` стоит ровно в одном `select`;
   - в job'е есть шаг с `if: matrix.shard == 'unit-b'` и строкой `run: cargo test
     --locked --workspace --no-fail-fast --doc`;
   - в job'е есть строка `run: cargo test --locked --workspace --no-fail-fast ${{
     matrix.select }}`;
   - маски `--test '<маска>'` из всех `select` разбивают имена ровно на одну
     маску на имя, по `fnmatch.fnmatchcase`. Это проверяется на каждом имени
     цели вида `test` из `cargo metadata --no-deps --format-version 1` и на
     синтетических именах `c + "x"` для каждого символа `c` из
     `string.ascii_letters + string.digits + "_"`, а также на однобуквенных
     именах `c`.
7. Последняя строка `PASS: …` перечисляет проверенное.

**Исход:** `python3 scripts/test-ci-test-output.py` на новом `ci.yml` печатает
`PASS`. Каждое из пяти внедрённых нарушений даёт ненулевой код (I5, I12):

- убрать один `--skip` из `unit-a`;
- заменить `'[!a-r]*'` на `'[s-z]*'`;
- добавить в `windows-core` строку `cargo test -p ide`;
- заменить в `run_tests` `grep -Fxq` на `true`;
- убрать `--list` из первого вызова cargo в `list_tests`.

После каждой пробы правка откатывается.

### Шаг 5. Очередь и таймауты

В `ci.yml` после `on:` добавляется блок:

```yaml
concurrency:
  # A newer push to a pull request supersedes its running check; push runs on
  # develop each get their own group and are never cancelled.
  group: ${{ github.event_name == 'pull_request' && format('ci-pr-{0}', github.event.pull_request.number) || format('ci-run-{0}', github.run_id) }}
  cancel-in-progress: true
```

`timeout-minutes: 60` стоит у `lint`, `check`, четырёх Windows-job'ов
и `windows-build`, это уже вписано в шаги 2 и 3. 60 минут покрывают холодную
сборку: release.yml собирает Windows из холодного кэша за 17:13 (запуск
38136007958), а зависание держит слот час вместо 6 ч. У `corpus-contract`
и `partitioned-baseline-scale` таймаут не меняется.

**Исход:** в `ci.yml` ровно один ключ `concurrency:` на верхнем уровне и семь
вхождений `timeout-minutes: 60` на уровне job'а (I8, I9).

### Шаг 6. Локальная сверка, коммит и замер на GitHub

1. Локальная сверка, все команды из корня worktree:
   - `python3 scripts/test-ci-test-output.py` → `PASS`, затем пять отрицательных
     проб шага 4;
   - `git show 22146bb3:.github/workflows/ci.yml > <scratch>/old.yml`;
   - `diff <(python3 -I <scratch>/ci_selections.py <scratch>/old.yml windows)
     <(python3 -I <scratch>/ci_selections.py .github/workflows/ci.yml windows)` —
     пусто;
   - то же в режиме `linux-gates` — пусто;
   - положительный контроль сверки: удалить из нового файла одно имя массива
     `mcp_server_names` у `windows-mcp-contract` — `diff` показывает ровно одну
     строку; откатить;
   - база: `cargo test --workspace --no-run` и `cargo test --workspace --examples
     --no-run`. Вторая команда нужна, потому что `--examples` собирает примеры
     в тестовом режиме, а первая — в обычном (проба 5). Затем для каждого `select`
     ноги (часть до `--`) `cargo test --workspace <select> --no-run` → строк
     `Compiling` 0 (I2). Проба при составлении плана: после такой базы `cargo test
     --workspace --test '[n-r]*' --examples --no-run` собрал 0;
   - отрицательные случаи `list_tests` на настоящем cargo, каждый → код не 0:
     игнорируемый тест (`list_tests --exact partitioned_baseline_scale --workspace
     --test partitioned_baseline_scale -- loads_and_classifies_1_6m_entries_with_bounded_rss`),
     чужой бинарник (`list_tests --exact bsl_search --workspace --lib --
     workspace_lease::tests::contention_unclaimed_and_missing_are_transient`) и
     отсутствующее имя (`list_tests --exact mcp_server --workspace --lib --
     no_such::test`);
   - `cargo test --workspace --lib --bins -- --list | grep -c ': test$'` равно
     сумме того же счёта с фильтрами `unit-b` и со `--skip` из `unit-a` (I1);
   - сумма `grep -c 'Executable tests/'` по трём маскам равна счёту для `--tests`
     (I1);
   - шаги «Named …» обеих ног запускаются локально: блок `run:` сохраняется в файл
     и выполняется `bash <файл>` → код 0.
2. `cargo fmt --all -- --check` и `cargo clippy --all-targets --all-features -- -D
   warnings` проходят: код не меняется, проверка обязательна по `CLAUDE.md`.
3. Коммит `ci: shard the PR check to finish within ten minutes` по Conventional
   Commits, обычный, с хуками. Затем `git push origin pio/ci-github-10min`.
4. Замер. Запуски R1 и R2 занимают по 13 слотов, на время замера они могут
   задержать чужие PR. Каждый запуск делается так:
   - запомнить `T=$(date -u +%Y-%m-%dT%H:%M:%SZ)` и `H=$(git rev-parse HEAD)`;
   - `gh workflow run ci.yml --repo itrous/bsl-analyzer --ref pio/ci-github-10min`;
   - id запуска — `gh run list --repo itrous/bsl-analyzer --workflow ci.yml --branch
     pio/ci-github-10min --event workflow_dispatch --json
     databaseId,createdAt,headSha --jq "[.[] | select(.headSha == \"$H\" and
     .createdAt >= \"$T\")] | .[0].databaseId"`. Пустой ответ значит, что запуск ещё
     не появился: команда повторяется, пока id не появится. Найденный id
     записывается и дальше используется только он.

   Первый запуск — R1. Новые ключи кэша в нём холодные, в зачёт он не идёт, но
   должен быть зелёным. После завершения R1 делается второй запуск, R2. Он
   восстанавливает кэши, сохранённые R1 в области ветки.
5. По R2 снимаются числа: `gh run view <R2> --json jobs` (длительность каждого
   job'а и `startedAt − createdAt`) и строки `Finished … in` из логов
   `gh api --allow-escape-sequences repos/itrous/bsl-analyzer/actions/jobs/<id>/logs`.
   Числа заносятся в сообщение коммита итоговой правки или в отчёт этапа:
   компиляция, прогон и итог по каждому job'у.
6. Приёмка замера. Каждый job пути PR (`lint`, пять ног `check`,
   `corpus-contract`, `windows-mcp-contract`, `windows-mcp`, `windows-analyzer`,
   `windows-core`) завершился зелёным, его длительность ≤ 9:00, а
   `startedAt − createdAt` ≤ 0:30, то есть очереди не было. Иначе — шаг 7.

### Шаг 7. Перебалансировка, только если в R2 есть job длиннее 9:00

Правила применяются к самому долгому job'у по порядку. После правки — шаг 6.1–6.6
(R3 — один запуск: кэши уже есть). Не больше двух итераций. Если после второй job
пути PR длиннее 10:00 — остановка со статусом `Нужно решение` и замером R2/R3 в
отчёте.

| слишком долгий job | правка |
|---|---|
| `check (unit-a)` | Фильтр `graph::state::` добавляется в фильтры `unit-b` и в `--skip` `unit-a` (локально 132 с суммы, на CI ~22 с) |
| `check (unit-b)` | `state::sync::` убирается из фильтров `unit-b` и из `--skip` `unit-a` (121 с, ~20 с) |
| `check (tests-a-m)` | Переезжает `m` (32 с): у `tests-a-m` маска `'[a-m]*'` → `'[a-l]*'`, у `tests-n-r` нижняя граница первой маски `n` → `m`, верхняя остаётся какой есть |
| `check (tests-rest)` | Переезжает `s*` (171 с) в новую ногу: добавляется нога `tests-s` с `select: --test 's*'`, у `tests-rest` маска `'[!a-r]*'` → `'[!a-s]*'` (+1 job) |
| `check (tests-n-r)` | Переезжает `r` (88 с): у `tests-n-r` в первой маске верхняя граница `r` → `q`, у `tests-rest` добавляется `--test 'r*'` |
| `windows-analyzer` | Шаг «Shared LSP and MCP owner and editor lifecycle» уходит в новый job `windows-shared-lsp` с тем же заголовком |
| `windows-mcp` | Шаг «Superseded owner lifecycle across MCP targets» уходит в новый job `windows-mcp-lifecycle` |
| `windows-core` | Шаг «Selective baseline path security» уходит в новый job `windows-ide` |
| `windows-mcp-contract` | Шаг «Vector lifecycle and fenced search construction» переезжает в `windows-core` |

После перебалансировки проверка 6 скрипта обязана проходить. Число job'ов
и раздел «Параллельность» пересчитываются в отчёте: +1 job на каждый новый.

### Шаги по тестам T1–T6

Выполняются после шага 7, по порядку. Каждый T-шаг — отдельный коммит
(`test(<крейт>): …` или `ci: …`), с хуками. Перед коммитом проходят `cargo fmt
--all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`
и `cargo test --locked --workspace --no-fail-fast`. Замеры «до» и «после»
снимаются одной и той же командой на одной машине, числа идут в сообщение
коммита. Правило для всех T-шагов: тест из поимённых списков CI (массивы
`mcp_server_names`, `bsl_search_names` шага 2, кортежи Windows шага 3) не
переименовывается и не получает ignore. Если иначе нельзя — остановка со
статусом `Нужно решение`: снятие поимённой проверки решает владелец.

#### T1. Гонки и окна времени (класс R)

1. Известная гонка. Тест
   `state::sync::tests::continuous_events_do_not_reset_rescan_debt_backoff`
   (`crates/mcp-server/src/state/sync.rs:1644`) читает `RescanDebt::waiting`. Этот
   метод под `cfg(test)` смотрит общие флаги `FORCE_RESCAN_DEBT_WAITING`
   и `FORCE_RESCAN_DEBT_DUE`. Другие тесты ставят эти флаги, держа `env_lock()`
   (`sync.rs:2463`, `:2676`). Сам тест блокировку не берёт. При 16 потоках он упал
   на «the first retry is immediate» (запуск при составлении плана). Правка —
   первой строкой теста `let _env_lock = env_lock();`. Проверка: до правки
   прогон `mcp_server` lib при `--test-threads 16` повторяется до падения этого
   теста, но не больше 5 раз. После правки — 5 зелёных прогонов подряд. Если до
   правки за 5 прогонов падения нет, это записывается, и правка всё равно
   делается: причина установлена по коду.
1б. Вторая известная зависимость от скорости. При `opt-level 1` (проба T4)
   упал `native_check::compiler::tests::child_output_is_drained_but_kept_within_the_limit`
   (`crates/bsl-check-executor/src/native_check/compiler.rs:1301`): `stdout`
   пуст вместо `1234`. Лимит делится между потоками, 4 и 4 байта (`:545`,
   `:547`). Скрипт сначала пишет 5 байт в stderr. Переполнение stderr
   останавливает процесс, и `stdout` успевает или не успевает получить вывод — это
   зависит от скорости. Правка в тесте: stderr не переполняется (`printf err
   >&2; printf 1234567890`), ожидание `stderr == b"err"`, `stdout == b"1234"`,
   `truncated`. Проверка: исправленный тест зелёный 20 раз подряд при `opt-level
   0` и при `CARGO_PROFILE_DEV_OPT_LEVEL=1` в отдельном каталоге сборки.
2. Кандидаты из сканов тестового кода (вне `cfg!(timing_contract)`):
   - 15 сравнений `elapsed()` с границей: `crates/mcp-server/src/change_hub.rs:5952`, `:10514`;
     `check_executor_windows_tests.rs:178`, `:258`; `indexing_runtime_tests.rs:34`,
     `:339`; `diagnostics_state/session.rs:886`; `graph/build.rs:4624`;
     `graph/build/vector_lifecycle_tests.rs:459`, `:479`, `:490`, `:499`, `:523`;
     `crates/bsl-search/src/engine.rs:8989`;
     `crates/bsl-analyzer/tests/shared_workspace_backpressure.rs:433`;
   - 18 ожиданий с тайм-аутом, где исход — отсутствие события
     (`recv_timeout`/`wait_timeout` с `is_err()`/`Timeout`), в 10 файлах:
     `bsl-analyzer/tests/selective_diagnostics_baseline_lsp.rs` (4),
     `diagnostics_baseline_lsp.rs` (2), `platform_help_process.rs` (2),
     `tests/common/mod.rs` (2), `shared_workspace_backend.rs` (1),
     `bsl-analyzer/src/server.rs` (2),
     `mcp-server/src/graph/build/vector_lifecycle_tests.rs` (2),
     `workspace_lease.rs` (1), `broker/daemon.rs` (1),
     `mcp-server/tests/broker.rs` (1).

   Каждое место относится к одному из разрядов по правилу
   `docs/contributing/DEVELOPMENT_RULES.md` («Тесты и время»):
   - нижняя граница ожидания продукта (`elapsed >= …`) — остаётся, это класс W,
     шаг T2;
   - потолок зависания, который действует только как `hang_guard` или
     `ceiling` — остаётся;
   - верхняя граница, окно отсутствия события или пауза, расставляющая гонку, —
     заменяется ожиданием события. Если события, которое наблюдает тест, нет,
     утверждение уходит под `if cfg!(timing_contract)`. Тест, у которого
     кроме времени проверять нечего, получает
     `#[cfg_attr(not(timing_contract), ignore = "timing contract: …")]`.

   Пример третьего разряда: в `shared_workspace_backpressure.rs:433` «writer
   застрял» значит «250 мс без нового кадра». На медленной машине это окно
   возвращается раньше настоящего упора. Тогда `stalled_after < total` проходит,
   ничего не проверив.
3. Стресс-проба: `cargo test --locked --workspace --lib --bins --no-fail-fast --
   --test-threads 64` три раза подряд и `cargo test --locked --workspace
   --no-fail-fast -- --test-threads 32` один раз. Каждое падение разбирается до
   причины. Тест, который зависит от нагрузки, чинится в коде теста. Если это
   невозможно, он получает ignore по пункту 2, а для тестов из поимённых
   списков CI действует правило раздела.

**Исход T1:** в отчёте коммита — таблица всех мест п. 2 (место, разряд, что
сделано) и список тестов, переведённых в timing-contract. Стресс-проба зелёная.

#### T2. Ожидания таймеров продукта (W1–W4), по ответу на В1

1. Классификация до правок. Для каждого из 317 тестов дольше 1 с по nextest
   определяется, какую константу продукта ждёт тест (W1–W4), или записывается
   «не W» с причиной (CPU, процессы). Таблица идёт в отчёт.
2. В1-а (рекомендуется), шов. В прод-файле появляется метод `…_for_test`,
   который задаёт длительность для одного экземпляра или одного теста. Образец —
   `with_build_timeout_for_test` (`crates/mcp-server/src/diagnostics_state/lifecycle.rs:543-546`,
   `#[cfg(test)]`). Шов, который нужен из другого крейта (тесты `mcp-server`
   задают тайм-аут занятости `bsl-search`, тесты бинарника — тайм-аут пула), идёт
   под фичей `test-seams`. Фича включается только в `[dev-dependencies]`
   крейтов-потребителей. Значения по умолчанию и поведение продукта без шва не
   меняются. Тест класса W укорачивает ожидание до ≤ 50 мс. Ход теста при этом
   прежний: занятость, отказ подключения, истёкший TTL. Обычная сборка фичи не
   видит: `cargo tree -p bsl-analyzer -e normal,build,features | grep -c
   test-seams` → 0. Положительный контроль — тот же вызов с `-e
   normal,build,dev,features` → не 0.
3. В1-б, без прод-кода. Тест класса W получает `#[cfg_attr(not(timing_contract),
   ignore = "timing contract: waits out <константа>")]`. Тесты из поимённых
   списков CI правило раздела не трогает: они остаются как есть, медленными,
   и в отчёте перечислены отдельно.
4. Замер: nextest-сумма тестов W до и после; прогон `mcp_server` lib при
   `--test-threads 4` до и после.

**Исход T2:** таблица п. 1. При В1-а у теста класса W собственное время < 1 с
(nextest), и `mcp_server` lib при 4 потоках короче на сумму/4 снятых ожиданий.
Отклонение больше 20 % от этой оценки разбирается в отчёте.

#### T3. Одна интеграционная цель на крейт (класс B)

1. Файл `crates/<крейт>/tests/<имя>.rs` переносится в цель
   `crates/<крейт>/tests/<крейт_с_подчёркиваниями>_suite/` (`git mv`; в `main.rs`
   — `mod <имя>;` по алфавиту), если выполнено всё:
   - имя не стоит в `ci.yml` как `--test <имя>`. Сейчас таких 17 целей, они
     остаются отдельными;
   - в файле нет глобального состояния процесса и перезапуска себя: `grep -E
     'set_var|remove_var|install_platform_help|set_current_dir|^#!\[|global_allocator|current_exe|Command::new|CARGO_BIN_EXE|^\s*(pub )?mod [a-z_0-9]+;'`
     пуст;
   - в крейте таких файлов не меньше двух.

   При составлении плана критерию отвечают 203 файла в 13 крейтах: `ide` 112,
   `parser` 24, `ide-diagnostics` 16, `lexer` 10, `bsl-platform` 9,
   `mcp-server` 7, `cfg` 5, `syntax` 5, `hir` 4, `sdbl-hir` 4, `platform-help` 3,
   `bsl-analyzer` 2, `hir-def` 2. Итого 294 цели превращаются в 104. Пути
   относительно файла (`include_str!("fixtures/…")`, `expect_file![…]`) получают
   префикс `../`. В переносимых файлах `ide` таких мест 3. Пути через
   `CARGO_MANIFEST_DIR` не меняются.
2. Проба при составлении плана, без коммита. Временный `tests/zz_probe_all.rs`
   с `#[path]` на 112 файлов `ide` дал 1094 теста, столько же, сколько 112 целей
   по отдельности, и все прошли. Числа при `CARGO_INCREMENTAL=0`:
   - сборка — 5.3 CPU-с против 278 CPU-с у 112 отдельных целей;
   - прогон при 4 потоках — 10.4 с против 61 с у тех же целей подряд;
   - размер — один бинарник 97 МБ против ~62 МБ у каждой отдельной цели.
3. Сверка имён до коммита (I13). Для каждого перенесённого файла множество строк
   `--list` старой цели совпадает с множеством строк новой цели с префиксом
   `<имя>::`, если префикс снять. Отдельно сверяются `--list --ignored`.
   Положительный контроль: временно удалить один `#[test]` из перенесённого
   файла — сверка показывает ровно одно расхождение.
4. Маски ног `check` от этого не ломаются: имя `*_suite` попадает ровно под одну
   маску. Пересчёт I1 по шагу 6.1 даёт новые числа; их сумма по маскам по-прежнему
   равна счёту `--tests`. Кортежи Windows (приложение A) не меняются: слитые
   цели в `ci.yml` по имени не названы, а фильтры без `--exact` находят тест и
   под префиксом модуля. Это проверяет I5 при прогоне.
5. `docs/contributing/DEVELOPMENT_RULES.md` получает абзац: новый файл без
   глобального состояния процесса кладётся в `tests/<крейт>_suite/`.

**Исход T3:** 104 интеграционные цели. Сверка п. 3 пуста, её контроль
срабатывает. Сборка `cargo test --workspace --no-run -j 4`
(`CARGO_INCREMENTAL=0`) и прогон записаны до и после.

#### T4. CPU-тяжёлые тесты (класс C), по ответу на В2

Проба при составлении плана, отдельный каталог сборки, `CARGO_INCREMENTAL=0`, `-j 4`:

| профиль воркспейс-крейтов | сборка `--workspace --no-run` | прогон `cargo test --workspace -- --test-threads 4` |
|---|---|---|
| `opt-level 0` (сейчас) | 125 с, 619 CPU-с | 664 с, 839 CPU-с, все зелёные |
| `opt-level 1` | 587 с, 2187 CPU-с | 547 с, 385 CPU-с, 1 падение (см. T1.1б) |

`opt-level 1` для всего воркспейса экономит 117 с прогона и добавляет 462 с
сборки, на CI ~+13 мин. Поэтому вариант отклонён.

1. `retired_material` (48 с на CI) считает только стандартной библиотекой и
   зависит от `opt-level` своей цели. Проба: тот же файл в отдельном крейте
   (scratch, 4 ядра через `taskset -c 20-23`): `opt-level 0` — 45.2 с, `release`
   — 4.3 с. По В2 = «да» тест переезжает в новый член воркспейса
   `crates/retired-material-gate` (`publish = false`, только `[dev-dependencies]
   tempfile`). Корень обхода — `concat!(env!("CARGO_MANIFEST_DIR"),
   "/../ide-diagnostics")`. В корневом `Cargo.toml` появляется
   `[profile.dev.package.retired-material-gate] opt-level = 3`. Две ссылки
   `LICENSING.md:290` и `:414` получают новый путь. Проверки: все пять тестов
   файла переезжают вместе и проходят, включая синтетические контроли окна и
   обхода git. Положительный контроль корня — временно направить корень на пустой
   каталог: тест падает на «обход не нашёл ни одного файла», затем откат. Материал
   из истории для проверки не восстанавливается. Время записывается до и после.
2. `reparse`, `method_increment_churn`, `line_tokens` (53 с на CI) считают кодом
   `parser` и `ide`. Их ускорение — либо `opt-level` этих крейтов (цена — сборка,
   см. таблицу), либо меньше случайных правок за прогон (цена — покрытие). План
   оставляет их как есть.

#### T5. Потоки libtest, по ответу на В3

Только при В3 = «да» и после зелёной стресс-пробы T1. Ноги `unit-a`
и `unit-b` получают `-- --test-threads 8`. В `select` `unit-a` это добавляется
после `--skip`-списка, а у `unit-b` — после фильтров. Проверка — R4 (шаг T6) и
пять последовательных `workflow_dispatch` без падений. Падение — тест чинится по
T1.3; при втором падении другого теста T5 откатывается, и это записывается в
отчёт.

#### T6. Замер R4 и сокращение ног

1. После T1–T5 — коммит, push, один запуск `workflow_dispatch` (R4) по шагу
   6.4–6.6. Кэши уже есть.
2. Слитая нога оценивается по R4 так: подготовка + большая из двух компиляций
   + сумма двух прогонов. Если оценка ≤ 7:30 (запас 1:30 до порога 9:00), ноги
   сливаются: `unit-a` + `unit-b` → `unit` (`select: --lib --bins`, doc-тесты
   в ней же), `tests-n-r` + `tests-rest` → `tests-n-z` (`select: --test '[!a-m]*'
   --examples`). Новое разбиение снова проверяется I1 и I2. Гейты «Named …»
   переезжают в `unit` и `tests-n-z`. Затем ещё один запуск (R5) и приёмка шага
   6.6.
3. При слиянии обеих пар на PR идёт 9 job'ов вместо 11. На Free два
   одновременных запуска (18 job'ов) идут без очереди. Раздел «Параллельность»
   в отчёте пересчитывается.

**Исход T6:** числа R4 (R5) по каждому job'у, итоговое число ног и job'ов.

### Шаг 8. Сдача

Итоговые числа R2 (или R3), а после T-шагов R4 (R5) приводятся в отчёте этапа. Сдача — через `pio_merge`,
как обычно. Запуск CI этого PR — событие `pull_request` до влития. По нему
наблюдаются I7 (`partitioned-baseline-scale` и `windows-build` — `skipped`) и
длительность PR-пути (I6, вторично к R2). Job'ы `lint`, `windows-mcp-contract`,
`windows-analyzer` и `windows-core` в этом запуске холодные, если PR-запуск не
находит кэшей ветки. Его длительность в зачёт I6 не идёт, в отчёте она
называется отдельно. После влития первый push в develop холодный для новых ключей
(`lint`, `windows-mcp-contract`, `windows-analyzer`, `windows-core`,
`windows-build`): ожидается до ~30–40 мин, это разовая цена. PR, запущенные до
окончания этого push, тоже идут холодными по этим job'ам.

## Инварианты

- **I1. Покрытие Linux не меняется.** Вход — `ci.yml` после шага 2. Объединение
  целей и тестов пяти ног `check` равно выбору `cargo test --workspace` по
  умолчанию, и каждый тест попадает ровно в одну ногу. Что наблюдаем:
  - счёт `--list` для `--lib --bins` = счёт с фильтрами `unit-b` + счёт со
    `--skip` `unit-a` (сейчас 9450 = 235 + 9215);
  - счета `Executable tests/` по трём маскам в сумме равны счёту `--tests`
    (сейчас 170 + 55 + 69 = 294);
  - doc-тесты есть в `unit-b`, примеры собираются в `tests-n-r`.
- **I2. Сборка Linux та же.** Все команды `cargo test` в `check` используют
  `--workspace` и не содержат `-p`. После базы `cargo test --workspace --no-run`
  и `cargo test --workspace --examples --no-run` каждая команда ноги с `--no-run`
  собирает 0 крейтов.
- **I3. Гейты имён Linux сохранены и ничего не выполняют.** Множество кортежей
  гейтов после правки равно множеству точечных шагов до неё: 229 строк, `diff`
  пуст. `list_tests` на отсутствующем, игнорируемом или чужом имени даёт
  ненулевой код и не запускает cargo без `--list`.
- **I4. Покрытие Windows не меняется.** Множество кортежей (выбор cargo, фильтр,
  точность) во всех job'ах `windows-*` после правки равно множеству job'а
  `windows-mcp` до неё: 282 строки, `diff` пуст.
- **I5. Каждый фильтр каждого вызова Windows обязан выбрать прошедший тест.**
  `run_tests` с фильтром, не давшим ни одной строки `test … ok`, или с
  ненулевым кодом cargo даёт ненулевой код.
- **I6. Время PR-запуска.** Тёплый запуск R2 (R3): каждый job пути PR ≤ 9:00,
  очереди нет.
- **I7. Разделение PR и push.** На событии `pull_request` job'ы
  `partitioned-baseline-scale` и `windows-build` имеют `conclusion: skipped`. На
  push и `workflow_dispatch` они выполняются. Текст `partitioned-baseline-scale`
  совпадает с `ci.yml:883-897` базы.
- **I8. Очередь PR.** Новый запуск того же PR отменяет идущий. Запуски на push
  друг друга не отменяют: группа содержит `run_id`.
- **I9. Таймауты.** У `lint`, `check`, четырёх `windows-*` и `windows-build` на
  уровне job'а стоит `timeout-minutes: 60`.
- **I10. Кэш.** Все пять ног `check` восстанавливают ключ с префиксом
  `v0-rust-check-Linux-x64-`. Сохранять пытается только `unit-a`.
- **I11. Зелёный итог.** Все job'ы R2 зелёные. После влития зелёный и первый
  push-запуск develop.
- **I12. Скрипт проверки CI ловит нарушения.** Скрипт проходит на новом
  `ci.yml` и падает на каждом из пяти внедрённых нарушений шага 4.
- **I13. T3 не теряет и не добавляет тестов.** Для каждого перенесённого файла
  множества строк `--list` и `--list --ignored` старой цели и новой цели (с
  префиксом `<имя>::`) совпадают. Общий счёт тестов воркспейса
  (`cargo test --workspace -- --list | grep -c ': test$'`) до и после T3 равен.
- **I14. Нет зависимости от нагрузки.** Стресс-проба T1.3 зелёная после T1 и
  повторно после T3.
- **I15. Каждое ignore T-шагов названо.** Разность `cargo test --workspace --
  --list --ignored` до T1 и после T2 равна перечню отчётов T1 и T2. У каждого
  нового ignore причина начинается с `timing contract:`. Ни одно имя из
  поимённых списков CI не стало игнорируемым: гейты «Named …» и Windows
  зелёные.
- **I16. Обычная сборка не видит тестовых швов** (только при В1-а): `cargo tree -p
  bsl-analyzer -e normal,build,features | grep -c test-seams` → 0, с `dev` → не 0.
- **I17. Время тестов.** При В1-а по nextest нет теста класса W дольше 1 с.

## Таблица проверок

| обещание / инвариант | проверка | наблюдаемый исход |
|---|---|---|
| PR-запуск ≤ 10 мин (I6) | шаг 6.4–6.6: `gh run view <R2> --json jobs` | каждый job пути PR ≤ 9:00, `startedAt − createdAt` ≤ 0:30 |
| Обязательные проверки зелёные, поведение продукта не меняется (I11) | `gh run view <R2>`; после влития — `gh run list --workflow ci.yml --branch develop --limit 1` | `conclusion: success` у всех job'ов. В `git diff --stat 22146bb3` вне `.github/workflows/ci.yml`, `scripts/test-ci-test-output.py` и `docs/plans/` есть только `crates/*/tests/**`, модули `#[cfg(test)]`, `docs/contributing/DEVELOPMENT_RULES.md`, `Cargo.toml` по В2 и швы по В1-а. Для швов `git diff -U0 22146bb3 -- 'crates/*/src/**'` вне модулей `#[cfg(test)]` содержит только определения под `#[cfg(test)]` или `#[cfg(feature = "test-seams")]` и строки фичи |
| I1 покрытие Linux | шаг 6.1: счета `--list` и `Executable tests/` | 9450 = 235 + 9215; 170 + 55 + 69 = 294 (на момент правки — фактические числа) |
| I2 сборка Linux та же | шаг 6.1: `--no-run` по каждой ноге после базы `--workspace --no-run` и `--workspace --examples --no-run`; проверка 6 скрипта | `Compiling` 0; в `select` нет `-p` |
| I3 гейты имён Linux | шаг 6.1: `diff` приложения A в режиме `linux-gates` и отрицательные случаи `list_tests` на настоящем cargo; случаи `list_tests` в скрипте (шаг 4.3) | `diff` пуст; игнорируемый, чужой и отсутствующий случаи дают не 0; код 99 заглушки не встречается |
| I4 покрытие Windows | шаг 6.1: `diff` приложения A в режиме `windows` и его положительный контроль | `diff` пуст; удалённое имя даёт ровно одну строку |
| I5 гейт фильтров Windows | шаг 4.2: случаи `run_tests` в скрипте | все ожидаемые коды совпали |
| I7 разделение PR и push, перенос «Build binary» и native-сборки | блок `partitioned-baseline-scale` в `git diff 22146bb3 -- .github/workflows/ci.yml`; `gh run view <R2> --json jobs` (R2 — `workflow_dispatch`); до влития — `gh run view <id> --json jobs` запуска `pull_request` PR сдачи (шаг 8); после влития — первый push-запуск | блок не изменён; `windows-build` и `partitioned-baseline-scale` — success в R2 и на push, skipped в запуске `pull_request`; артефакт `native-check-windows-amd64` есть у R2 и push-запуска (`gh api repos/itrous/bsl-analyzer/actions/runs/<id>/artifacts`) |
| Push-запуск ~13–15 мин | `gh run view <R2> --json jobs` | release gate — 12–16 мин, остальные job'ы ≤ 9:00; после влития push-запуск по `createdAt`/`updatedAt` — 13–16 мин (кроме первого, холодного) |
| I8 очередь PR | блок `concurrency` в `ci.yml` (шаг 5); первый случай двух push подряд в один PR — PR сдачи, если в него будет второй push, иначе первый такой PR после влития | выражение группы совпадает с шагом 5; первый из двух PR-запусков получил `conclusion: cancelled`, второй дошёл до конца; push-запуски develop `cancelled` не получают |
| I9 таймауты | `grep -c '^    timeout-minutes: 60$' .github/workflows/ci.yml` | 7 |
| I10 кэш | логи R2: шаг `Swatinem/rust-cache@v2` каждой ноги `check` | строка восстановления с ключом `v0-rust-check-Linux-x64-…`; у ног кроме `unit-a` пост-шаг не сохраняет (`save-if` false) |
| I12 скрипт ловит нарушения | шаг 4, пять внедрённых нарушений | каждое — ненулевой код, после отката — `PASS` |
| I13 T3 без потерь | T3.3: сверка `--list` и `--list --ignored`, общий счёт | расхождений нет; удалённый для контроля `#[test]` даёт ровно одно |
| I14 нет зависимости от нагрузки | T1.3 после T1 и после T3 | все прогоны зелёные |
| I15 ignore названы | `comm` списков `--list --ignored` до T1 и после T2 против отчётов | совпадает; гейты «Named …» и Windows в R4 зелёные |
| I16 швы не в обычной сборке | `cargo tree` из T2.2 | 0 без `dev`, не 0 с `dev` |
| I17 время тестов | nextest до и после T2 | у тестов W < 1 с; `mcp_server` lib при 4 потоках — по исходу T2 |
| Полный прогон короче | `cargo test --workspace --no-run -j 4` и прогон с `--test-threads 4`, локально, до T1 и после T4 | числа в отчёте T6; сборка ≤ 60 % исходной после T3 |
| Оценка параллельности при нескольких PR | наблюдение после влития: `gh run view <id> --json jobs` у одновременных запусков | ждущие job'ы видны как `startedAt − createdAt` > 0:30; при ≤2 одновременных запусках итог ≤ 10:00 |

## Открытые вопросы

Часть A и T1, T3, T6 начинаются без ответа. На согласовании владелец выбирает
набор переносов из таблицы цен и отвечает на В1–В3. Без ответа T2, T4 и T5 не
выполняются.

- **В1. Тестовые швы в прод-файлах для ожиданий W1–W4.** В1-а (рекомендуется):
  метод `…_for_test` под `#[cfg(test)]` или фичей `test-seams`. Он ставится
  только в `[dev-dependencies]`, так что значения по умолчанию прежние, и обычная
  сборка шва не видит (T2.2). Выигрыш — ~480 с собственного времени тестов,
  `mcp_server` lib на CI ~209 → ~110 с. В1-б: без прод-кода тесты W получают
  ignore под timing-contract, кроме поимённых в CI. Цена — в таблице цен.
- **В2. Отдельный крейт для `retired_material` (T4).** Новый член воркспейса
  `crates/retired-material-gate` с `opt-level = 3` в профиле dev и новый путь
  в `LICENSING.md`. Выигрыш — 48 → ~5 с на CI. По умолчанию — нет, тест
  остаётся в `ide-diagnostics`.
- **В3. Больше потоков libtest в ногах `unit-*` (T5).** Выигрыш на CI
  оценивается в 1–2 мин на ноге `unit-a`. Цена — перегрузка 4 vCPU и риск
  нестабильности, которую по правилу 4ae4c791 придётся чинить или
  игнорировать. По умолчанию — нет.

- **«Build binary» и native-сборка.** По умолчанию обе на push-only
  `windows-build`. Варианты: «Build binary» в `windows-mcp-contract` (+2:06)
  и/или native-сборка с артефактом в `windows-core` (+1:28). Оба по оценке
  укладываются в 10:00.
- **Тесты Linux под `-p`.** Точечные тесты Linux больше не идут под фичами
  одного пакета. Если эта конфигурация нужна сама по себе, её дешевле всего
  вернуть отдельной ногой `check` с `cargo test --locked -p mcp-server -p
  bsl-search -p bsl-analyzer --no-run`. Это сборка без прогона, ~2–3 мин,
  плюс 1 job и ~0.3 GB кэша. По умолчанию не делается.
- **План аккаунта.** API его не показывает. На Free (20) гарантия ≤10:00
  держится для ≤2 одновременных запусков, на Pro (40) — для ≤3.

## Приложение A. Сверка кортежей `ci_selections.py`

Файл кладётся в scratch-каталог и в репозиторий не коммитится. Запуск:
`python3 -I ci_selections.py <ci.yml> windows|linux-gates`.

```python
#!/usr/bin/env python3
"""Print the (selection, filter, exact) tuples a CI workflow runs.

Usage: ci_selections.py <ci.yml> <windows|linux-gates>
Old layout: run_nonempty / run_exact; new layout: run_tests / list_tests.
"""
import re
import shlex
import sys

text = open(sys.argv[1], encoding="utf-8").read()
mode = sys.argv[2]

jobs = re.findall(r"^  ([a-zA-Z0-9_-]+):\n(.*?)(?=^  [a-zA-Z0-9_-]+:\n|\Z)", text, re.M | re.S)


def logical_lines(body):
    """Join backslash continuations and expand `name=( ... )` arrays used as "${name[@]}"."""
    body = re.sub(r"\\\n\s*", " ", body)
    arrays = {}
    for name, items in re.findall(r"^\s*(\w+)=\(\n(.*?)^\s*\)", body, re.M | re.S):
        arrays[name] = [l.split("#", 1)[0].strip() for l in items.splitlines() if l.split("#", 1)[0].strip()]
    out = []
    for line in body.splitlines():
        line = line.strip()
        for name, items in arrays.items():
            line = line.replace(f'"${{{name}[@]}}"', " ".join(items))
        out.append(line)
    return out


def old_tuples(line):
    words = shlex.split(line)
    head, args = words[0], words[1:]
    if head == "run_exact":
        if len(args) == 1:
            return [(("-p", "mcp-server", "--lib"), args[0], True)]
        return [(("-p", args[0], "--lib"), args[1], True)]
    exact = False
    if "--" in args:
        tail = args[args.index("--") + 1:]
        args = args[: args.index("--")]
        exact = "--exact" in tail
    sel, filters, i = [], [], 0
    while i < len(args):
        if args[i] in ("-p", "--bin", "--test"):
            sel += args[i:i + 2]
            i += 2
        elif args[i].startswith("-"):
            sel.append(args[i])
            i += 1
        else:
            filters.append(args[i])
            i += 1
    return [(tuple(sel), f, exact) for f in (filters or [None])]


def new_tuples(line):
    words = shlex.split(line)
    head, args = words[0], words[1:]
    exact = bool(args) and args[0] == "--exact"
    if exact:
        args = args[1:]
    binary = None
    if head == "list_tests":
        binary, args = args[0], args[1:]
    sel, filters = args, []
    if "--" in args:
        sel, filters = args[: args.index("--")], args[args.index("--") + 1:]
    key = tuple(sel) if binary is None else (binary,) + tuple(sel)
    return [(key, f, exact) for f in (filters or [None])]


def as_gate(row):
    """An old Linux point-step selection as the list_tests key that replaces it."""
    sel, f, exact = row
    sel = [w for w in sel if w != "--locked"]
    package, target = sel[1], sel[2:]
    if target == ["--lib"]:
        binary = package.replace("-", "_")
    elif target[0] == "--bin":
        binary = target[1].replace("-", "_")
    else:
        binary = target[1]
    return ((binary, "--workspace") + tuple(target), f, exact)


rows = set()
for name, body in jobs:
    windows = re.search(r"^    runs-on: windows", body, re.M) is not None
    if mode == "windows" and not windows:
        continue
    if mode == "linux-gates" and (windows or name not in ("check",)):
        continue
    for line in logical_lines(body):
        word = line.split(" ", 1)[0]
        if word in ("run_nonempty", "run_exact") and not line.endswith("{"):
            found = old_tuples(line)
            rows.update(map(as_gate, found) if mode == "linux-gates" else found)
        elif word in ("run_tests", "list_tests") and not line.endswith("{"):
            rows.update(new_tuples(line))
for row in sorted(rows, key=repr):
    print(row)
```

На базовом `ci.yml` команда даёт 282 строки в режиме `windows` и 229 в режиме
`linux-gates`.
