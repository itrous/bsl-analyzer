# unit-a: инвентарь ожиданий `mcp-server/graph` и `change_hub`

## Область и метод

База среза: `94924aaef7d0fae9f91d6000a0efb6a98d4ff6ce` (`pio/fix-unit-a-hang`). Инвентаризированы unit-тесты `crates/mcp-server/src/graph/**` и `crates/mcp-server/src/change_hub.rs`, которые участвуют в `workspace --lib --bins`; исключены тесты под `state::overlay_`, `state::bootstrap::`, `state::embed::`, `state::sync::`. Проверены `recv`, condition-variable waits, joins, process waits, async waits, deadlines и циклы polling. Продуктовая логика не менялась.

Для обычного профиля `test_utils::wait::hang_guard(bound)` задаёт не менее 10 минут; в `timing_contract` он сохраняет `bound`. `poll_until` теперь ограничен этим конечным deadline независимо от числа опросов. Требование детерминированности из `DEVELOPMENT_RULES.md` соблюдено: положительные ожидания завершаются ожидаемым событием, а не более быстрым CPU.

## Исправления

- `crates/mcp-server/src/graph/test_support.rs:74-106,114-129,166-181` — общие graph-ожидания используют `WAIT_GUARD = hang_guard(30s)` и `poll_until`; сохранён `WAIT_CEILING` как alias, чтобы не менять многочисленные тестовые вызовы. `hold_every_handle` теперь ждёт свободный пул по deadline, захватывая найденные handles внутри успешной проверки. `wait_for_child:78-101` ограничивает process waits и убивает/reaps зависший тестовый subprocess.
- `crates/mcp-server/src/change_hub.rs:5858-5904` — `STATE_CEILING = hang_guard(60s)`, `eventually` использует deadline-based `poll_until`; `Patience` проверяет `Instant` deadline. `wait_step:5889-5901` ограничивает `Condvar::wait_timeout` меньшим из шага и оставшегося времени. Удалены счётчик wakeups и мёртвые переменные у callsites `PollGate`/`AnnounceBarrier:4086-4220`; уведомления больше не продлевают ожидание. Второй аргумент `Patience::new` сохранён как совместимость с исключёнными sibling state tests, которые используют общий test helper; в целевом наборе он не участвует в deadline.
- `crates/mcp-server/src/graph/build.rs:6266` — ожидание READY от дочернего процесса теперь имеет 60s timing bound через `hang_guard`; выход/ошибка чтения также заканчивают ожидание. `mkfifo` в тесте `:7972-7984` переведён на ограниченный `wait_for_child`. Три тестовых `wait_until_within` заменили `ceiling` на `hang_guard` (`:6982-6987,7341-7346,11484-11489`).
- `crates/mcp-server/src/graph/state.rs:3344,4470,5031,5412,11143` — прямые channel/deadline и явные wait bounds конечны. Для «spent budget» тест теперь ждёт observable release асинхронного kick перед проверкой статуса (`:3564-3576`), а не делает отрицательную проверку через короткое число быстрых polls.
- `crates/mcp-server/src/graph/snapshot.rs:2250,2703,2827` — finite receive/reload bounds используют `hang_guard`.
- `crates/mcp-server/src/graph/portable_workspace_graph_tests.rs:51-53,339-363` и `graph/build/vector_lifecycle_tests.rs:2078-2094` — socket read и два исполняемых unit-test subprocess ждут конечный guard; дочерние результаты всё ещё проверяют exit status и `1 passed`.
- `eventually_within` оставлен только для отрицательного timing-contract сценария `a_quiet_delivery_is_listed_only_once_the_ledger_has_it`. В ordinary-профиле весь тест явно `ignore` с причиной и точной opt-in командой; при явном запуске проверка не вырезается cfg. Использующийся в ней 20-минутный watchdog — страховка самого timing-only окна, а не основной deadline положительного ожидания.

## Полный инвентарь ожиданий и освобождений

### Общие graph helpers и graph state

- `graph/test_support.rs:114-129,166-181` — `hold_every_handle`, `wait_until` и `wait_until_within` опрашивают состояние до guard; при неуспехе сообщается graph summary. `:417-432` — `wait_for_hub_seq_above` ограничен `Patience` и ждёт конкретный hub sequence.
- `graph/state.rs:3344-3355` — delivery worker шлёт событие после захвата ledger; тест ограниченно ждёт старт/наблюдение, затем отпускает ledger и join-ит worker. `:3929` — parked executor turn освобождается `release.store(true)` до `first.join`; `:4040` — parked flush также получает release перед join. Предварительные ожидания `parked` защищены `eventually`.
- `graph/state.rs:4470-4480` — четыре owner-thread выполняют конечные серии из 40 операций, после чего каждый посылает завершение; общий 90s `hang_guard` защищает приём всех четырёх сигналов до joins.
- `graph/state.rs:5362` — `WorkspaceLease::hold_cache_lock_for(..., 5s)` самостоятельно отпускает lock по конечному сроку; join следует за проверкой, что lease уже стала доступна.
- `graph/state.rs:6267,6288,6520,6548,6579,6607` — publish hooks ограниченно ждут сигналов/release на `STATE_CEILING`; соответствующие тесты вызывают `window.release()` либо посылают release-канал перед возвратом и join (`:6553,6629-6630`). Hook timeout превращается в assert failure, а не в вечное ожидание.
- `graph/state.rs:3355,3929,4040,4480,5362,6553,6629-6630,7081,9498` (до добавления guards) — joins после release/завершения gated probe/executor turn дополнительно защищены `crate::test_wait::join`; нарушение release-протокола теперь завершает тест по hang guard. Входные условия защищены конечными graph/change-hub helpers.
- `graph/state.rs:4243-4258,4342-4357,8249-8260,10779-10812,11110-11118` — циклы в hooks ждут hub fact, release или handoff через `Patience`; каждый имеет конечный state deadline. В циклах с коротким `sleep` возможно превышение deadline на один шаг планировщика.

### `change_hub.rs`

- Продуктовый `HubThread::stop`: `:1908-1941` — постановка Shutdown ограничена `STOP_BUDGET`; затем thread проверяется через `is_finished` в пределах того же deadline. Только уже завершённый handle join-ится; при превышении срока handle detach-ится с warning, а не зависает вызывающий.
- Продуктовые condition-variable waits `:2937,2997,4013` получают timeout/deadline вызывающего. Hub-loop receives `:4483,4676` ограничены оставшимся временем до очередного scheduled poll.
- Тестовые readiness/wait helpers `:2690-2718,2949-2950,5873-5904` используют конечный `STATE_CEILING`/`Patience`; тестовые ожидания проходят через `eventually` или `wait_step` с deadline.
- `HubHold::wait` и test seams `:147-166,2116,2672` сами безвременны, но их release принадлежит тестовому `HubHoldGuard`, чей `Drop` вызывает `release` (`:454-472`). Это RAII rendezvous, не самостоятельное ожидание события из продукта.
- `:6168` noise worker завершает цикл после `stop.store(true)`; затем join. `:8024` resubscribe worker отпускается `drop(blind)` перед join; до этого тест имеет конечную `Patience`. `:9881-9894` declaration signal имеет `STATE_CEILING`, reconcile barrier явно `release`-ится до join, а `rearm` ограничен собственным timeout. `:9997` waiter join следует сразу после `hub.interrupt_waiters()`, а последующие waits проверяют, что interruption сохраняется. `:10838` drop test ограниченно ждёт done channel с `hang_guard(STOP_BUDGET * 3)`.
- `eventually_within` в `:5917-5930` применяется только из timing-only отрицательной проверки, указанной выше.

### `graph/build.rs`, `snapshot.rs`, portable и subprocess tests

- `graph/build.rs:6230-6269` — child crash-test намеренно ждёт stdin в `announce_and_wait`; родитель ограниченно ждёт READY, затем явно убивает ребёнка. `PatchChild::Drop:6279-6285` повторно убивает и reaps его. `:7972-7984` — `mkfifo` ждёт конечный process guard; SQLite projection worker шлёт результат через channel, приём защищён `STATE_CEILING` (`:7998-8001`).
- `graph/build.rs:4499,4567,4603` — joins publisher в тестах установки replacement; публикацию освобождают возврат всех held readers. Первые три stand-теста помечены `ignore` в обычном профиле и запускаются только как timing-contract сценарии.
- `graph/snapshot.rs:495,606` — product `GraphStore` condition waits получают явный timeout и deadline. `:1937-1971` — три worker rendezvous-ятся на `Barrier(3)`, затем выполняют drain/release/invalidate и join-ятся. `:2114` giver join следует после read с 10s bound и завершения заданного 100ms release. `:2250-2252` worker сигнализирует вход, receive защищён `hang_guard(1s)`, после чего join.
- `graph/portable_workspace_graph_tests.rs:40-129` — тестовый HTTP server в nonblocking accept-loop завершает работу по stop atomic; drop соединяется с listener, streams имеют read timeout, затем guarded thread join. `:339-363` — child test runner использует `child_output` с guard60s: stdout/stderr читаются параллельно ожиданию выхода, поэтому полный pipe не блокирует child; assertion проверяет, что действительно выполнен ровно выбранный тест. Аналогично исправлен lifecycle capture wrapper; self-subprocess regression control захватывает1MiB из каждого pipe.
- `graph/build/vector_lifecycle_tests.rs:2078-2094` — аналогичный дочерний test runner для обычного capture-теста ограничен 60s guard. Issue441 helpers и тела `#[ignore]` в этом файле не менялись и в обычный unit-a запуск не входят.

### Проверенные циклы без unit-test hang wait

- `graph/watcher.rs:180-…` — production watcher повторяет readiness/watch до stop/terminal состояния; каждое readiness/change ожидание получает конечный slice или change-hub timeout. `graph/build.rs:1801-1830` повторяет публикацию replacement при занятых readers, но retry sleep stop-aware и завершается при stop/terminal lease. Циклы в `graph/debt.rs:306,2744,3028,3050,4704` — обход/пересчёт структур данных, без блокирующих receives, joins или необусловленного сна.

## Риски и границы

- `Barrier::wait` в `graph/snapshot.rs:1945,1958,1965` не имеет timed API, но ни один такой wait не выполняется на основном потоке теста: все три worker completion joins защищены `crate::test_wait::join` (10 минут). Потеря участника теперь приводит к сообщению сторожа вместо бесконечного ожидания теста; зависшие workers при panic отсоединяются.
- Дополнительные completion guards применены ко всем runnable graph/state/snapshot joins, к portable HTTP fixture Drop, capture probe и change-hub noise/resubscribe/declaration/interruption workers. Продуктовый join уже завершённого HubThread не менялся; timing-only ignored publisher tests вне scope.
- `PatchChild::Drop` и `wait_for_child` делают `wait()` после успешного kill для reap; обычный OS kill освобождает процесс, но неисправность самого OS/process supervisor не имеет второй независимой границы.
- Issue441 scale/smoke bodies в `vector_lifecycle_tests.rs` помечены `#[ignore]`; по указанию scope не менялись. Их helpers содержат отдельные длинные/временные waits и не считаются runnable unit-a. При явном запуске этих ignored-тестов остаются отдельные риски, включая `Command::output()` для `git rev-parse` (`:381-386`), обычный `ceiling` в scale embedding fixture и длинные deadline.
- Результат `mkfifo` теперь bounded, но сам тест предполагает наличие Unix `mkfifo`; отсутствие программы завершается ошибкой запуска, не timeout.
- `cargo`, тесты, сборка и commit не выполнялись по прямому требованию. Выполнен только `rustfmt --edition 2021 --config skip_children=true --check` по изменённым Rust-файлам и `git diff --check`; поведенческие/компиляционные риски остаются до разрешённого запуска unit-a.
