# unit-a: инвентарь ожиданий вне `mcp-server` и `test-utils`

## Область и метод

Срез исходной инвентаризации: `94924aaef7d0fae9f91d6000a0efb6a98d4ff6ce`. Проверены unit-тесты в `crates/**/src/**` для `--lib --bins`; исключены MCP-сервер и `test-utils`. Unit-a также пропускает имена `state::overlay_`, `state::bootstrap::`, `state::embed::`, `state::sync::`. Integration targets не менялись и здесь не перечисляются.

Прочитан раздел «Тесты и время» в `docs/contributing/DEVELOPMENT_RULES.md:325-359`: обычный исход не должен зависеть от скорости/планирования; положительное ожидание завершается наблюдаемым событием; верхние границы и окна отсутствия события относятся к `timing_contract`. Для hang-only ожиданий использован `hang_guard` (10 минут в обычном профиле, сохранён прежний timing-предел). Проверены `recv`, `join`, Barrier, async timeout, deadlines, `sleep`/`yield_now`, `ceiling`/`NO_BOUND` и timing-cfg. Product-логику не менял.

## Исправленные ожидания

### `ceiling`/`NO_BOUND` заменены на `hang_guard`

- `crates/bsl-analyzer/src/task_pool.rs:200,261,270` — watchdog результатов пула и broadcast.
- `crates/bsl-analyzer/src/handlers/dispatch.rs:757`; `handlers/request.rs:1718,2482,2486,4352`; `handlers/workspace_batch.rs:500` — test gates, event waits и task recheck.
- `crates/bsl-analyzer/src/lsp_live_vfs_propagated_tests.rs:12,57`; `server.rs:2630-2828,2908,3510-3726,4369,4401,4734,4832,5399,5523,7161-7331`; `workspace.rs:1699,1711,1766,1780` — response/task/loader waits и owner readiness.
- `crates/bsl-analyzer/src/call_hierarchy_index_state_tests.rs:15,64,167,175,201,261,282` — completion waits.
- `crates/bsl-search/src/store.rs:8741`, `publish.rs:469`, `engine.rs:6845` — watchdog-константы test-модулей; `embedder/payload_tests.rs:37` — test-server I/O.
- `crates/ide-host-core/src/workspace.rs:894,907-1002` — test waits и bounded completion workers.

Без зависимости на `test-utils` сохранены 60 секунд в timing profile, обычный предел изменён с century/NO_BOUND на 10 минут: `crates/bsl-debug/src/session.rs:1380-1384`, `crates/project-model/src/lib.rs:4462-4466`, `crates/project-model/src/diagnostics_baseline_fs.rs:365-369`, `crates/vfs-notify/src/lib.rs:1203-1207`.

### Голые приёмы и локальные серверы

- `crates/bsl-analyzer/src/workspace.rs:1699,1711,1766,1780` — блокирующие приёмы заменены на `recv_timeout(hang_guard(30s))`.
- `crates/bsl-analyzer/src/call_hierarchy_index_state_tests.rs:166,172` — сообщения worker/editor ограничены watchdog-ом; joins следуют за completion-сигналами.
- `crates/bsl-analyzer/src/server.rs:533`, `handlers/dispatch.rs:892` — синхронно отправленные в том же вызове ответы проверяются `try_recv`.
- `crates/bsl-search/src/engine.rs:13106,13111,13166` — сигналы lock/control в SQLite tests ограничены `HANG_WATCHDOG`.
- `crates/bsl-search/src/engine/lifecycle_tests.rs:158-193,1312-1330` — fake HTTP servers используют nonblocking accept с общим deadline и socket read/write timeout; связанные joins тем самым ограничены.

## Барьеры и связанные worker-ы

В unit-test коде `Barrier` заменены event-channel протоколами: readiness → start/release → completion, с `recv_timeout(hang_guard)` и joins только после completion. Это сохраняет конкурентный старт, но не оставляет peer навечно внутри `Barrier`, если участник не дошёл до rendezvous.

- `crates/ide-host-core/src/workspace.rs:889-1002` — reader admission/release, writer start/completion и две конкурентные diagnostics sweep заменены каналами. Внешние ожидания, включая результат независимого worker, имеют hang guard; joins следуют за результатами.
- `crates/bsl-analyzer/src/call_hierarchy_index_state_tests.rs:299-324` — reader readiness/start/completion через каналы; публикация по-прежнему проверяется одновременно с чтением старого/нового индекса.
- `crates/bsl-search/src/engine.rs:13830-13884` — два reference writer-а синхронизируются каналами и возвращают результаты до joins.
- `crates/bsl-search/src/progress.rs:338-414` — contention reader и 8 pass-id workers используют сигналы/результатные каналы с watchdog-ом; убран 20ms sleep из lock contention test.
- `crates/bsl-launcher/src/use_cases.rs:882-972` — два 8-worker relink/swap теста используют readiness/start/completion channels.
- `crates/intern/src/lib.rs:234-286` — 8 spelling interns синхронизируются каналами; worker threads обычные detached handles, поэтому timeout приёмника не входит в автоматический unbounded join `thread::scope`.

Единственный оставшийся в этих crate-ах `Barrier::wait()` — `crates/bsl-analyzer/src/task_pool.rs:59-66`: это product task-pool startup rendezvous, не test Barrier, не менялся по прямому указанию.

## bsl-check-executor: анализ и защита product-test ожиданий

Добавлена только test-side инфраструктура в `crates/bsl-check-executor/src/native_check/tests.rs:7-74`: `guard_future`, `guard_task`, `wait_until`, `wait_until_sync` и guarded `wait_child`, все используют существующий `test_utils::wait::hang_guard` (10 минут обычно / 60 секунд timing). `test-utils` crate не менялся. Добавлены его dev-dependency записи в `crates/bsl-check-executor/Cargo.toml`, `crates/bsl-launcher/Cargo.toml`, `crates/intern/Cargo.toml` и соответствующие package dependency entries в `Cargo.lock`.

- Public-check и root-lock async tests: `native_check/tests.rs:111-213`, `root_lock.rs:358-454`. Внешние futures и task joins имеют outer guard; `RootLock::acquire` также получает собственный deadline. 20/30ms sleeps в cancellation setup заменены на `yield_now`.
- External process tests в `native_check/compiler.rs:1299-1930` и extension/process tests в `profile.rs:3796-4755` ждут через outer guard. `run_bounded`, `run_bounded_with_log`, `run_designer_xvfb`, `update_extension_properties`, `verify_copy_extensions` и cleanup futures дополнительно имеют product deadline/cancellation/cleanup protocol; outer hang guard закрывает регрессию, обходящую этот protocol. Spawned Tokio tasks ожидаются через `guard_task` (при таймауте abort), а readiness/cleanup polling ограничен `wait_until`.
- В product supervisor `compiler.rs:565` `child.wait()` участвует в `tokio::select!` вместе с cancellation и run deadline; Windows `terminate_unadmitted_child` ограничивает `child.wait()` через `timeout_at` (`:647`). Unix и Windows `ProcessGroupGuard::stop` опрашивают статус/группу до stop deadline (`:936-1003`). Следовательно, эти production waits ограничены протоколом; product-код не менялся.
- Прямые `std::process::Child::wait()` в тестах заменены `wait_child` (`tests.rs:45-74`) — nonblocking `try_wait` loop под hang guard. При истечении срока child убивается, а reaper переводится в detached worker, так что test thread не блокируется на OS wait. В X11 zombie test `x11.rs:552-568` сначала под guard наблюдается `/proc` zombie, затем `waitid` вызывается с `WNOHANG`; reaping тоже проходит через guarded helper.
- X11 fixture process waits (`x11.rs:421-502`), process group fixture readiness/cleanup (`profile.rs:4641-4753`) и `compiler.rs` readiness тесты имеют hang-guard polling. Cmd.exe `command.output()` в Windows test обёрнут в guard; `kill_on_drop(true)` чистит child при timeout.
- В `engine.rs:13920-13977` также guard-нуты readiness и wait spawned test-process children; child wait использует `try_wait` под общим watchdog, на timeout child kill-ится и reap переносится в detached worker.

Остаточный риск указан явно: общий helper `wait_child` не может отменить OS процесс в uninterruptible kernel wait; при timeout test thread завершается с ошибкой, посылает kill и отделяет reaper. В product fallback `compiler.rs:1008` для targets вне Unix/Windows есть прямые `kill().await`/`wait().await` без локального deadline; process-supervision unit tests компилируются только для Unix, Windows имеет отдельный deadline-guarded путь, поэтому этот fallback не является ожиданием unit-a на поддерживаемых тестовых targets. Это portability risk product-кода, не менялся по запрету менять продукт.

## Прочие ожидания и timing-only ветки

- `crates/bsl-analyzer/src/task_pool.rs:284,308` — test workers ждут закрытия `gate_rx`; sender закрывается тестом после проверки full (`:297,328`). Это ограниченный владением gate. `:59-66` — единственный product Barrier, см. выше.
- `crates/ide-host-core/src/workspace.rs:791-804` — production helper `hold_shared_test_gate` содержит опциональный cross-process file-release polling loop с 5ms sleep без локального deadline; он активируется только env-маркерами shared-backend harness. Код не менялся из-за запрета менять product logic; штатная команда unit-a эти маркеры не задаёт.
- `crates/bsl-analyzer/src/lsp_live_vfs_propagated_tests.rs:52-59,521-538,691-706` — query owner освобождается `release_owner.send` после Salsa cancellation либо panic-path release; waits и связанные joins имеют guard/event completion.
- `crates/bsl-analyzer/src/handlers/request.rs:3258,3355,3408-3480,4390` — publisher/waiter/follower/hover/contender worker-ы имеют response/completion channels с hang guard либо bounded waiter policy; joins идут после получения результата.
- `crates/bsl-analyzer/src/server.rs:5491,5498-5505` — observer join после sync VFS writer; observer сам ограничен watchdog loop.
- `crates/bsl-analyzer/src/bin/cli/lsp.rs:620,690`; `bin/cli/logging/journal.rs:724-728,844-848,912-914` — async guards/receives; joins after release/completion. `journal.rs:762-770` — latency assertion только timing profile.
- `crates/bsl-search/src/publish.rs:1015-1049`, `store.rs:9369,9442-9445` — release/blocked channels имеют `HANG_WATCHDOG`; joins идут после отправки release/transaction commit.
- `crates/bsl-search/src/engine/lifecycle_tests.rs:303,347,390,472,1369,1410,1442` — fake server joins теперь ограничены accept/socket guards (`:158-193,1312-1330`). `engine.rs:10048,14361` — local server join после stop/запросов; `:13106-13124,13160-13181` — lock-holder получает control/admission signals и commit перед join.
- `crates/bsl-search/src/embedder/payload_tests.rs:37,99,367` — guarded socket server; Drop включает stop, будит accept и joins worker. `progress.rs:345-412` — joins после guarded completion.
- `crates/bsl-search/src/lifecycle.rs:909,975,1019,1177`; `crates/platform-help/src/package.rs:482,486`; `crates/lexer/src/lib.rs:814,838,926`; `crates/ide-db/src/type_kernel.rs:163-164`; `crates/stdx/src/par_guard.rs:71` — конечная работа/worker completion, без Barrier и release gate. `crates/bsl-launcher/src/use_cases.rs:923,971` и `crates/intern/src/lib.rs:282` — joins после guarded results.
- `crates/bsl-search/src/external_baseline/postgres.rs:6206-6233,6634-6663` — DB race tests координируют republisher atomic window и 3s sleep перед join; это timing-sensitive нижняя граница/race fixture, не unbounded wait. Осталось отдельным риском, если задача будет расширена до устранения всех временных предпосылок.
- Локальные timing assertions остаются только под `cfg!(timing_contract)`: `bsl-debug/src/session.rs:1775,1893-1894,2109`; `vfs-notify/src/lib.rs:1897-1902`; `ide/src/graph.rs:3196-3198`; `hir-def/src/call_hierarchy_index/tests.rs:114-119`; `bsl-search/src/store.rs:8991`, `point_refresh.rs:650`, `lifecycle.rs:914,926,1024,1123,1148`, `engine/lifecycle_tests.rs:1241,1288`, `engine.rs:8988`. Тестовые watchdog константы без общей зависимости остаются в `bsl-debug/src/session.rs:1380-1384`, `project-model/src/lib.rs:4462-4466`, `project-model/src/diagnostics_baseline_fs.rs:365-369`, `vfs-notify/src/lib.rs:1203-1207`.
- False positive `Duration::MAX`: `bsl-analyzer/src/call_hierarchy_index_service_tests.rs:161` использует `passes: 0`, то есть немедленно исчерпанный бюджет, не ожидание; `call_hierarchy_index_service/mod.rs:25` — product sentinel; `bsl-platform/tools/html-parser/src/lib.rs:342` — product grace period.

## Exclusions and validation

`crates/mcp-server/**`, `crates/test-utils/**` и integration tests не менялись. Integration false positives намеренно не включены: это вне scope.

Cargo и тесты не запускались, коммиты не создавались. Выполнен прямой `rustfmt --edition 2021 --config skip_children=true` и статическая проверка diff. Компиляция и поведение изменений остаются неподтверждёнными до разрешённого запуска unit-a.
