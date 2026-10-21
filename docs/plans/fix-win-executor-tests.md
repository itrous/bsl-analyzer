# План: детерминированные Windows-тесты check-executor'а

## Задача

Установить причину синхронного падения 4 тестов в
`crates/mcp-server/src/check_executor_windows_tests.rs` на Windows-раннере
GitHub Actions и составить план починки.

Факт падения: develop-прогон 39579663947, job «Windows MCP transports + secure
broker» (id 119153845445), шаг `MCP library lifecycle, workspace cache and editor
regressions`, `cargo test -p mcp-server --lib`, `test result: FAILED. 66 passed;
4 failed` за 23,82 с:

| Тест | Паника |
|---|---|
| `cancellation_hard_stops_the_live_job_and_its_descendant` | `:243:5` «executor should create its marker» |
| `event_is_signaled_and_cooperative_child_finishes_within_grace` | `:169:5` «fake executor should create its marker» |
| `timeout_hard_stops_job_and_reaps_its_descendant` | `:202:5` «executor should create its marker» |
| `valid_bad_wire_nonzero_exit_and_output_limit_keep_caller_priority` | `:298:5` `left: Error`, `right: Valid` |

Требования владельца: тесты, зависящие от окружения и времени, чинятся
детерминированно — без таймеров и без привязки к скорости CPU (правило
4ae4c791); `ignore` допустим только для неисправимого и здесь не цель. Причина
устанавливается доказательством. Критерий готовности: job windows-mcp зелёный,
4 теста проходят независимо от скорости запуска исполнителя.

## Вне рамок

- Продуктовый код check-executor'а (`crates/mcp-server/src/tools/check_executor.rs`)
  не меняется: продукт ведёт себя по контракту (см. «Причина»).
- Linux-тесты исполнителя в `check_executor.rs` (модуль `tests`) и
  `check_executor_wire_tests.rs` — не трогаются.
- `queued_cancellation_does_not_spawn_the_executor` не меняется: его
  `sleep(50 мс)` перед отменой (`check_executor_windows_tests.rs:277`) не
  расставляет гонку — отмена до и после входа в очередь даёт один и тот же исход
  (`Err` без запуска), см. `check_executor.rs:73-86`.
- Скорость старта Python на раннере (холодный кэш, сканирование образа) — это
  окружение, его не лечим и не ускоряем; тесты перестают от неё зависеть.
- Конфигурация CI (`.github/workflows/ci.yml`) на develop не меняется: фильтр
  `check_executor_windows_tests` уже стоит в шаге windows-mcp
  (`ci.yml:781`, вызов `ci.yml:795`).
- Ветка разведки `pio/fix-win-executor-tests-diag` на origin (коммит
  `60e174dd`) — не вливается; удаление опубликованной ветки остаётся за
  владельцем.

## Допущения, принятые без вопроса

1. Причина — в тестовой фикстуре, а не в продукте: продукт по истечении своего
   дедлайна обязан вернуть `transport_timeout` и сделал ровно это (доказательство
   ниже). Поэтому фикс — тестовый, и отдельный круг ревью продуктового среза не
   нужен.
2. Ожидание «наступит ли событие» строится по правилам репозитория
   (`docs/contributing/DEVELOPMENT_RULES.md:324-354`): положительное ожидание
   заканчивается самим событием, оборона от зависания —
   `test_utils::wait::hang_guard` (`crates/test-utils/src/wait.rs:19-30`,
   10 минут в обычном профиле), нижние границы `elapsed >= …` остаются.
3. Для тестов, где дедлайн или окно grace продукта не должны «обогнать» старт
   исполнителя, используются приостановленные часы Tokio
   (`#[tokio::test(start_paused = true)]`) и задокументированное свойство Tokio:
   пока выполняется задача `spawn_blocking`, автопродвижение часов запрещено
   (tokio 1.52.1 `src/time/clock.rs:127-135` «Preventing auto-advance»,
   реализация `src/runtime/blocking/schedule.rs:24-26,44-48`). Прецедент
   приостановленных часов в крейте — `crates/mcp-server/src/broker/transport.rs:429-437`;
   `tokio` с `test-util` уже в dev-зависимостях (`crates/mcp-server/Cargo.toml:101-102`).
4. Проверочные прогоны на Windows идут на отдельной ветке разведки с
   одноjob'овым `ci.yml` через `workflow_dispatch` (как прогон 39604450208);
   итоговое подтверждение — обычный CI pull request'а задачи.

## Устройство сейчас

### Фикстура и тесты

- Все четыре упавших теста запускают fake-исполнитель на Python
  (`check_executor_windows_tests.rs:15-93`), программа — `python` из `PATH` или
  `BSL_TEST_PYTHON` (`:95-97`). Оба не упавших теста Python не исполняют:
  `queued_cancellation_…` не доходит до запуска (`:263-281`),
  `nested_ui_job_…` проверяет отказ допуска до `ResumeThread` (`:327-565`).
- `profile()` задаёт продуктовый дедлайн `timeout: 3 с` (`:103`), `stop_grace`
  250 мс (`:107`), `reap_timeout` 2 с (`:108`).
- `wait_for_file` ждёт появления маркера не дольше 5 с, опрос шагом 10 мс
  (`:121-129`). Маркер fake пишет только после старта интерпретатора, импорта
  `ctypes/json/subprocess`, проверки `IsProcessInJob` и чтения stdin до EOF
  (`:16-27`, `:45`, `:47-48`, `:69-71`).
- `event_…` (`:155-180`): ждёт маркер (`:169`), отменяет, требует `Err`
  (`:177`), `elapsed >= 250 мс` по `std::time::Instant` (`:178`) и код выхода 0
  (`:179`) — fake выходит сам после события, `sleep(0.05)` и печати ответа
  (`:52-58`); grace продукта 300 мс (`:161`).
- `timeout_…` (`:182-228`): продуктовый дедлайн 5 с (`:188`), ждёт маркер и
  маркер потомка по 5 с (`:202-206`), требует `transport_timeout` (`:217`) и коды
  выхода 1 у родителя и потомка (`:226-227`).
- `cancellation_…` (`:230-261`): дедлайн 3 с из `profile()`, ждёт оба маркера
  (`:243-247`), отменяет, требует `Err`, `elapsed >= 250 мс`, коды 1 (`:256-260`).
- `valid_…` (`:283-325`): первый вызов в режиме `valid` под дедлайном 3 с обязан
  дать `Status::Valid` (`:287-301`), затем `malformed`, `exit1-valid`,
  `overflow` дают свои коды отказа (`:303-324`).
- `wait_for_process_exit` ждёт выхода не дольше 3 с (`WaitForSingleObject(…, 3_000)`,
  `:148`). Fake ждёт событие остановки не дольше 30 с с `assert`
  (`:54`, `:77`), режимы `job-hang`/`overflow` и потомок «висят» `time.sleep(30)`
  (`:67`, `:79`, `:90`). В `nested_ui_…` ещё два верхних срока: выход
  дочернего теста 8 с (`:409`) и возврат разрешения 3 с (`:377`).

### Продукт (не меняется)

- `check()` фиксирует дедлайн `Instant::now() + profile.timeout` до очереди
  (`tools/check_executor.rs:69`), `Instant` — `tokio::time::Instant` (`:17`).
- `execute()` запускает исполнитель (`:163`) и ждёт `biased`-select: отмена,
  дедлайн, переполнение, завершение (`:202-208`). По дедлайну — `stop_child` и
  `caller_failure(…, "transport_timeout", …)` (`:226-239`); `caller_failure`
  ставит `Status::Error` любому коду, кроме `context_required`/`unsupported`
  (`:686-690`).
- `stop_child`: `SetEvent` (`:567`, `:660-666`), ожидание выхода до
  `grace_deadline` и досыпание до него (`:575-587`), затем, если не дождались,
  `TerminateJobObject(job, 1)` (`:597-599`, `:668-677`), ожидание reap
  (`:603-612`).
- Windows-запуск: Job с `KILL_ON_JOB_CLOSE` (`:416-417`), процесс создаётся
  `CREATE_SUSPENDED` (`:465`), назначается в Job (`:468`), главный поток
  возобновляется (`:505`).
- Контракт допускает любой ненулевой дедлайн (`crates/check-contract/src/lib.rs:226-240`);
  Linux-тест уже ставит 1 час, чтобы старт исполнителя не гонялся с дедлайном
  (`tools/check_executor.rs:1403`), и ждёт готовность, падая, если `check`
  кончился раньше (`:1411-1430`).

### Причина — доказательства

Механизм (общий для обеих групп): время от запуска `python` до первой записи
fake'а на свежем Windows-раннере непостоянно и иногда превышает фиксированные
окна тестов — 5 с ожидания маркера и 3 с продуктового дедлайна в `profile()`.

1. **Группа «маркер не создан»** (`event_…`, `cancellation_…`, `timeout_…`):
   `wait_for_file` вернул `false` по своему 5-секундному сроку (`:122`), пока
   fake ещё не дошёл до записи маркера. В прогоне 119153845445 тесты стартуют
   вместе с бинарём (`running 72 tests` в 09:49:52.5) и падают на 6,16 / 7,28 /
   10,47 с от старта — после 5-секундного ожидания; в зелёных прогонах того же
   образа `cancellation_…` завершается за 1,31–2,84 с (job'ы 119135873448,
   119103952543, 119123514079, 119155416997, 119215220572; таймлайн снят
   скриптом по логам `gh run view --job <id> --log`).
2. **`Error` вместо `Valid`**: первый вызов `valid_…` не уложился в дедлайн 3 с
   (`:103`), продукт вернул `transport_timeout` → `Status::Error`
   (`check_executor.rs:226-239`, `:686-690`). Таймлайн 119153845445: тест
   стартовал не раньше 09:49:57.8 (освобождение потоков после `nested_ui_…` и
   `queued_…`), упал в 09:50:02.15 — ≈4,3 с, что равно 3 с дедлайна + 250 мс
   grace + Job-остановка и reap; мгновенные отказы (`spawn_failed`,
   `transport_exit_status`) вернулись бы за доли секунды.
3. **Воспроизведение на Windows-раннере** (прогон 39604450208, job
   «Windows MCP executor diagnostics», образ `windows-2025-vs2026`
   20261018.275.1, Python 3.12.10 из `C:\hostedtoolcache`): в копию теста
   добавлены задержка старта fake'а `BSL_DIAG_STARTUP_DELAY` и печать кода
   отказа.
   - Холодный прогон без задержки: 6 passed; маркер первых запусков — через
     1,02–1,08 с, первый `valid` целиком — 1,003 с.
   - Тёплый прогон без задержки: 6 passed; маркеры — 76–102 мс, `valid` — 77,6 мс;
     отдельные запуски `python -c "import ctypes, json, os, subprocess, sys, time"`
     после прогона — 96–101 мс (4 параллельно) и 68–70 мс (подряд).
     Холодный старт дороже тёплого на порядок.
   - Положительный контроль, задержка 6 с: `FAILED. 2 passed; 4 failed` — ровно
     те же четыре теста с теми же паниками: три «executor should create its
     marker» после `found=false after 5.00 s` и
     `valid status=Error failure=Some("transport_timeout") after 3.268 s`.
4. **Флейк, а не регрессия коммита.** Коммит прогона 39579663947 (`de74e072`)
   меняет только `save-if` у `rust-cache` в `ci.yml`. Следующий прогон develop
   (39580134142) и все последующие зелёные. Падения этой группы в истории
   job'а windows-mcp (выгрузка неудачных прогонов `ci.yml` с 2026-10-15,
   `gh run list -L 200` → 68 неудачных, из них 23 с упавшим windows-mcp):
   6 job'ов — 117982567805 (4 теста), 118019545286 (3), 118168572724 (3),
   118575628301 (3), 119153845445 (4), 119184349669 (1). Частота: с 2026-10-19
   в `ci.yml` 113 прогонов, job windows-mcp — 73 success, 11 failure,
   28 cancelled (`gh api …/runs/<id>/jobs` по каждому); все 6 падений группы
   приходятся на эти 84 завершённых job'а, около 7 %. Тесты пришли в develop
   2026-10-19 (merge `60ad8264`, PR #450: первый коммит first-parent истории
   develop, содержащий `181f0207`), и группа падает с первого же дня
   (117982567805 — 2026-10-19T09:19Z). После этого тестовый файл не менялся
   (`git log -- crates/mcp-server/src/check_executor_windows_tests.rs` →
   `726d877b`, `181f0207`), а правки `check_executor.rs` — `bcfd7bea` и
   `8e1af83c` — лежат в Unix-модуле `tests`; путь запуска на Windows не
   менялся. В старой раскладке CI (шаг «Windows check executor
   lifecycle tests», только 6 этих тестов, без соседней нагрузки) падения те же
   (`running 6 tests … FAILED. 3 passed; 3 failed … 5.35s`, job 118575628301) —
   причина не в параллельной нагрузке соседних тестов, а в старте Python.
5. Положение Python: на раннере первым в `PATH` стоит
   `/c/hostedtoolcache/windows/Python/3.12.10/x64/python`, второй —
   `WindowsApps/python` (вывод `type -a python`, прогон 39604450208).

## Шаги

Все правки шагов 1–2 — только в `crates/mcp-server/src/check_executor_windows_tests.rs`.
Модуль компилируется лишь под Windows (`crates/mcp-server/src/lib.rs:8-10`),
поэтому компиляция и clippy проверяются на Windows-раннере (шаг 3).

### Шаг 1. Фикстура без верхних сроков

1. `FAKE_EXECUTOR`:
   - `kernel.WaitForSingleObject(event, 30_000) == 0` в режимах `event-exit`
     (`:54`) и `job-hang` (`:77`) → `kernel.WaitForSingleObject(event, 0xFFFFFFFF) == 0`
     (INFINITE): ожидание кончается событием либо остановкой Job.
   - В строку импорта (`:16`) добавить `threading`; `time.sleep(30)` в
     `job-hang` после события (`:79`) и в `overflow` (`:90`) →
     `threading.Event().wait()`. В `child_code` начало
     `"import ctypes,os,sys,time; "` (`:62`) → `"import ctypes,os,sys,threading; "`,
     хвост `time.sleep(30)` (`:67`) → `threading.Event().wait()`. Процессы
     живут, пока их не остановит Job; при падении теста их убивает закрытие Job
     (`KILL_ON_JOB_CLOSE`, `check_executor.rs:416-417`).
   - `time.sleep(0.05)` в `event-exit` (`:56`) не трогать — это не срок, а
     задержка ответа после отмены.
2. `profile()`: `timeout: Duration::from_secs(3)` (`:103`) →
   `timeout: Duration::from_secs(60 * 60)` — дедлайн продукта не гоняется со
   стартом исполнителя (как `tools/check_executor.rs:1403`).
3. `wait_for_process_exit` (`:148`): срок `3_000` →
   `u32::try_from(test_utils::wait::hang_guard(Duration::from_secs(3)).as_millis()).unwrap()`.
4. `wait_for_file` (`:122`, остаётся для `nested_ui_…`): `Duration::from_secs(5)` →
   `test_utils::wait::hang_guard(Duration::from_secs(5))`. В `nested_ui_…`:
   `Duration::from_secs(8)` (`:409`) → `test_utils::wait::hang_guard(Duration::from_secs(8))`,
   `Duration::from_secs(3)` (`:377`) → `test_utils::wait::hang_guard(Duration::from_secs(3))`.
5. Новый помощник готовности (рядом с `wait_for_file`; импорты
   `std::sync::atomic::{AtomicBool, Ordering}`):

   ```rust
   /// Waits on a blocking thread until every marker exists. While a blocking task runs, a
   /// paused Tokio clock does not auto-advance, so no product deadline or grace window can
   /// expire while the executor is still starting.
   struct Readiness {
       stop: Arc<AtomicBool>,
       handle: tokio::task::JoinHandle<bool>,
   }

   impl Readiness {
       fn spawn(paths: Vec<PathBuf>) -> Self {
           let stop = Arc::new(AtomicBool::new(false));
           let handle = tokio::task::spawn_blocking({
               let stop = stop.clone();
               move || {
                   let bound = test_utils::wait::hang_guard(Duration::from_secs(5));
                   let started = std::time::Instant::now();
                   loop {
                       if paths.iter().all(|path| path.exists()) {
                           return true;
                       }
                       if stop.load(Ordering::Relaxed) || started.elapsed() >= bound {
                           return false;
                       }
                       std::thread::sleep(Duration::from_millis(10));
                   }
               }
           });
           Self { stop, handle }
       }

       async fn wait<T: std::fmt::Debug>(mut self, task: &mut tokio::task::JoinHandle<T>, what: &str) {
           tokio::select! {
               biased;
               ready = &mut self.handle => assert!(ready.unwrap(), "{what}"),
               early = &mut *task => panic!("{what}: check ended before the executor was ready: {early:?}"),
           }
       }
   }

   impl Drop for Readiness {
       // Runtime shutdown waits for blocking tasks; a failed test must not keep polling.
       fn drop(&mut self) {
           self.stop.store(true, Ordering::Relaxed);
       }
   }
   ```

   Исход опроса не зависит ни от числа опросов, ни от шага
   (`DEVELOPMENT_RULES.md:339-341`); ранний конец `check` — немедленная
   содержательная паника с результатом, а не срок.

### Шаг 2. Тесты

1. `valid_bad_wire_nonzero_exit_and_output_limit_keep_caller_priority` — без
   изменений тела; детерминирован шагом 1.2 (дедлайн 1 час) и 1.1 (`overflow`
   висит до Job-остановки).
2. `cancellation_hard_stops_the_live_job_and_its_descendant` — обычные часы.
   Перед `tokio::spawn` создать
   `let ready = Readiness::spawn(vec![marker.clone(), marker.with_file_name("processes.txt.child")]);`,
   задачу сделать `let mut task = …`, вместо двух `wait_for_file` (`:243-247`) —
   `ready.wait(&mut task, "executor should create its marker and its child marker").await;`.
   Остальное как сейчас: `Err`, `started.elapsed() >= Duration::from_millis(250)`,
   коды выхода 1 и 1. Fake не выходит сам (шаг 1.1), поэтому коды 1 — только от
   `TerminateJobObject(job, 1)`.
3. `timeout_hard_stops_job_and_reaps_its_descendant` →
   `#[tokio::test(start_paused = true)]`, `profile.timeout = Duration::from_millis(500)`
   (`:188`). `Readiness::spawn(…оба маркера…)` вызвать ДО `tokio::spawn(check …)`;
   `ready.wait(&mut task, "executor should create its marker and its child marker").await;`
   Затем БЕЗ `.await`: прочитать PID'ы, проверить `"1"`, открыть оба процесса и
   вызвать `tokio::time::resume();`. Дальше как сейчас: `transport_timeout`,
   потомок в Job, коды 1 и 1. Почему так: пока идёт опрос готовности, часы стоят
   (задача `spawn_blocking`), поэтому дедлайн продукта отсчитывается от
   `resume()`, то есть строго после готовности; между пробуждением теста и
   `resume()` нет точки, где рантайм мог бы простаивать.
4. `event_is_signaled_and_cooperative_child_finishes_within_grace` →
   `#[tokio::test(start_paused = true)]`. `let grace = profile.stop_grace;`
   (300 мс). `Readiness::spawn(vec![marker.clone()])` до `tokio::spawn`, затем
   `ready.wait(&mut task, "fake executor should create its marker").await;`.
   Затем БЕЗ `.await`: прочитать PID, проверить `"1"`, открыть процесс и
   запустить ожидание выхода на блокирующем потоке:
   `let process = open_process(parent) as usize;`
   `let exit = tokio::task::spawn_blocking(move || wait_for_process_exit(process as windows_sys::Win32::Foundation::HANDLE));`
   (`HANDLE` — сырой указатель и не `Send`). Потом
   `let started = tokio::time::Instant::now(); token.cancel();`,
   `assert!(task.await.unwrap().is_err(), "cancellation takes precedence over JSON");`,
   `assert!(started.elapsed() >= grace, "caller observes full stop grace");`,
   `assert_eq!(exit.await.unwrap(), 0, "Event stop exited without Job termination");`.
   Почему так: пока блокирующая задача ждёт выхода fake'а, приостановленные
   часы не идут, и окно grace продукта не может истечь до кооперативного
   выхода; Job-остановка поэтому не может опередить выход с кодом 0 ни на какой
   машине. `elapsed` меряется часами Tokio — продукт досыпает до
   `grace_deadline` по ним же (`check_executor.rs:585-587`).
5. Коммит: `test(mcp-server): make Windows check executor tests independent of executor startup time`.

Наблюдаемый исход шагов 1–2: в файле нет верхнего срока, кроме
`hang_guard`; правка только в одном тестовом файле.

### Шаг 3. Проверка на Windows-раннере (разведочная ветка, 1 job)

1. Поверх коммита шага 2 собрать отдельный коммит, НЕ на ветке задачи (через
   временный индекс и `git commit-tree`, как `60e174dd`), и опубликовать его
   обычным push в `refs/heads/pio/fix-win-executor-tests-verify`. В нём:
   - `.github/workflows/ci.yml` — один job на `windows-latest`
     (`workflow_dispatch`): checkout, `dtolnay/rust-toolchain@stable` с
     `components: clippy`, `Swatinem/rust-cache@v2` с `save-if: false`, затем
     шаги ниже, каждый `set +e` и печать `exit=`.
   - Диагностические хуки в `FAKE_EXECUTOR` (только в этом коммите):
     сразу после `mode, marker = sys.argv[1:3]` две строки —
     `time.sleep(float(os.environ.get('BSL_DIAG_STARTUP_DELAY', '0')))` и
     `if os.environ.get('BSL_DIAG_EXIT_BEFORE_MARKER'): sys.exit(7)`;
     в `event-exit` последнее `sys.exit(0)` →
     `sys.exit(int(os.environ.get('BSL_DIAG_EVENT_EXIT_CODE', '0')))`;
     в `job-hang` сразу после `assert kernel.CloseHandle(event)` —
     `if os.environ.get('BSL_DIAG_HANG_EXIT_CODE'): sys.exit(int(os.environ['BSL_DIAG_HANG_EXIT_CODE']))`.
     Fake'и наследуют окружение тестового процесса (`check_executor.rs:457-465`
     добавляет только `BSL_CHECK_STOP_EVENT`).
2. Шаги job'а и ожидаемый вывод:
   - `cargo clippy -p mcp-server --lib --tests -- -D warnings` → exit 0.
   - без переменных: `cargo test -p mcp-server --lib -- check_executor_windows_tests`
     → `6 passed; 0 failed`.
   - `BSL_DIAG_STARTUP_DELAY=6` → `6 passed; 0 failed` (на коде develop тот же
     вход дал `2 passed; 4 failed`, прогон 39604450208).
   - `BSL_DIAG_EXIT_BEFORE_MARKER=1` → `2 passed; 4 failed`: `event_…`,
     `cancellation_…`, `timeout_…` с паникой «check ended before the executor was
     ready» и `transport_exit_status` в тексте, `valid_…` с `left: Error`; шаг
     завершается за минуты, без ожидания `hang_guard`.
   - `BSL_DIAG_EVENT_EXIT_CODE=3` → падает только `event_…` на
     `left: 3, right: 0`.
   - `BSL_DIAG_HANG_EXIT_CODE=5` → падают `cancellation_…` и `timeout_…` на
     коде выхода родителя `left: 5, right: 1` (родитель проверяется первым, до
     потомка, поэтому шаг не ждёт `hang_guard`).
3. Запуск: `gh workflow run ci.yml --ref pio/fix-win-executor-tests-verify`; один
   прогон, без серий. Результат и номер прогона записать в отчёт исполнения.
   Ветка не вливается.

### Шаг 4. Сдача

1. Опубликовать ветку задачи и открыть PR в develop обычным порядком pio.
2. CI PR: job «Windows MCP transports + secure broker» зелёный, в шаге `MCP
   library lifecycle, …` строки `check_executor_windows_tests::<имя> ... ok` для
   всех шести тестов.
3. Записать разобранный сбой `pio memory note` (сделано на этапе плана,
   см. ниже) — исполнителю не повторять.

## Инварианты

- **I1** Старт исполнителя любой длительности (вход: `BSL_DIAG_STARTUP_DELAY`
  0 и 6 с) → все 6 тестов `check_executor_windows_tests` проходят.
- **I2** Исполнитель умер до маркера (вход: `BSL_DIAG_EXIT_BEFORE_MARKER=1`) →
  `event_…`, `cancellation_…`, `timeout_…` падают сразу с «check ended before the
  executor was ready», а не по сроку; `valid_…` падает на `Error`.
- **I3** `valid_…`: режимы `valid`/`malformed`/`exit1-valid`/`overflow` →
  `Status::Valid` / `transport_malformed_response` / `transport_exit_status` /
  `transport_output_limit`.
- **I4** `event_…`: отмена живого fake'а → `Err`; `elapsed` по часам Tokio
  `>= stop_grace`; fake вышел сам с кодом 0 (вход-контроль:
  `BSL_DIAG_EVENT_EXIT_CODE=3` → тест падает).
- **I5** `cancellation_…`: отмена → `Err`; `elapsed >= 250 мс`; родитель и
  потомок в Job (`"1"`), оба завершены Job-остановкой с кодом 1 (вход-контроль:
  `BSL_DIAG_HANG_EXIT_CODE=5` → тест падает).
- **I6** `timeout_…`: дедлайн после готовности → `transport_timeout`; потомок
  унаследовал Job; родитель и потомок завершены с кодом 1 (вход-контроль:
  `BSL_DIAG_HANG_EXIT_CODE=5` → тест падает).
- **I7** В обычном профиле в `check_executor_windows_tests.rs` нет верхних
  сроков ожидания, кроме `test_utils::wait::hang_guard`.
- **I8** Продуктовый код не меняется.

## Таблица проверок

| Обещание / инвариант | Проверка | Ожидаемый исход |
|---|---|---|
| I1 | шаг 3: тесты без переменных и с `BSL_DIAG_STARTUP_DELAY=6` | `6 passed; 0 failed` оба раза |
| I2 | шаг 3: `BSL_DIAG_EXIT_BEFORE_MARKER=1` | `2 passed; 4 failed`, три паники «check ended before the executor was ready», `valid_…` `left: Error` |
| I3 | шаг 3 без переменных: `valid_…` | `... ok` |
| I4 | шаг 3: без переменных и `BSL_DIAG_EVENT_EXIT_CODE=3` | `event_… ok`; при коде 3 — `FAILED`, `left: 3` |
| I5 | шаг 3: без переменных и `BSL_DIAG_HANG_EXIT_CODE=5` | `cancellation_… ok`; при коде 5 — `FAILED`, `left: 5` |
| I6 | шаг 3: без переменных и `BSL_DIAG_HANG_EXIT_CODE=5` | `timeout_… ok`; при коде 5 — `FAILED`, `left: 5` |
| I7 | `rg -n -e 'from_secs\(' -e 'from_millis\(' -e '_000\)' -e 'sleep\(30\)' crates/mcp-server/src/check_executor_windows_tests.rs` | каждое совпадение — одно из: параметр профиля продукта (`timeout`, `stop_grace`, `reap_timeout` в `profile()`, `stop_grace = 300 мс` в `event_…`, `timeout = 500 мс` и `stop_grace = 250 мс` в `timeout_…`), аргумент `hang_guard(…)`, шаг опроса 10 мс, `sleep(50 мс)` в `queued_…`, нижняя граница 250 мс в `cancellation_…`; `30_000`, `3_000` и `sleep(30)` не встречаются |
| I8 | `git diff --stat origin/develop...HEAD` | изменён только `crates/mcp-server/src/check_executor_windows_tests.rs` (и этот план) |
| windows-mcp зелёный, 4 теста детерминированы | шаг 4: CI PR, job windows-mcp | job `success`, шесть строк `check_executor_windows_tests::… ... ok` |

## Открытые вопросы

Нет.
