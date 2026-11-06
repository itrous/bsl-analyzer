# #780: протокол разведки без изменения Rust

База: `95cce624e2163eb6cc9381b04295f27ed58b3be7` (`HEAD = origin/develop` при начале разведки).
Дата: 2026-11-06. Linux. Все удаления ниже относятся **только к собственному target этой worktree**; чужие процессы, сборки и бинарники не менялись. Rust-исходники не изменялись. Проба LD_PRELOAD — внешний измерительный инструмент, не предлагаемая реализация теста. После запусков каталог `.artifacts/780` перенесён без изменения содержимого в `/home/itrous/src/.temp/issue780-plan-evidence-12749/780`; ниже сохранены исходные пути команд. Самодостаточные команды и выдержки этого документа входят в git, сырые логи и исполняемые файлы — нет.

## Контроль под нагрузкой

```sh
cargo test -p bsl-analyzer --test shared_workspace_backend --no-run
PROBE_OUTPUT="$PWD/.artifacts/780/control" LD_PRELOAD="$PWD/.artifacts/780/probe.so" \
 timeout 900 cargo test -p bsl-analyzer --test shared_workspace_backend \
 invalid_project_config_reports_error_and_keeps_last_good_shared_view -- --ignored --exact --nocapture
```

Вывод (`.artifacts/780/control/run.log:8-18`):

```text
running 1 test
shared backend test: attach_order=s19-invalid-config-last-good phase=opening the caller buffer
shared backend test: attach_order=s19-invalid-config-last-good phase=opening the target buffer
shared backend test: attach_order=s19-invalid-config-last-good phase=last-good shared owner resolves the known definition
shared backend test: attach_order=s19-invalid-config-last-good phase=invalid project config reported while preserving last-good scope
shared backend test: attach_order=s19-invalid-config-last-good phase=frozen last-good owner still answers definition and hover
shared backend test: attach_order=s19-invalid-config-last-good phase=valid repair attached the preserved LSP session to a new owner epoch
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 2.09s
```

Exit 0. `uptime`: `17:43:21 ... load average: 91,60, 83,31, 59,90`. Это наблюдение при существовавшей нагрузке, не доказательство отсутствия гонок и не требование к машине.

## Детерминированный вход: исполняемый файл удалён перед repair

После сборки имя тестового бинарника из вывода cargo было `target/debug/deps/shared_workspace_backend-9e4a9883436fe3ee`. В чистой сборке брать имя из её собственного вывода. Сохранить исполняемый файл отдельной **копией**, не hardlink. Проба ниже при записи checkpoint `frozen last-good owner still answers definition and hover` синхронно удаляет путь **до возврата write**. Следующий шаг теста — запись исправного TOML. Поэтому здесь нет гонки между наблюдателем stderr и repair.

```sh
mkdir -p .artifacts/780/unlink
cp --reflink=auto target/debug/bsl-analyzer-app .artifacts/780/app.saved
PROBE_OUTPUT="$PWD/.artifacts/780/unlink" \
 PROBE_UNLINK="$PWD/target/debug/bsl-analyzer-app" \
 LD_PRELOAD="$PWD/.artifacts/780/probe.so" \
 timeout 20 strace -f -s 256 -e trace=execve,readlink,openat,clock_nanosleep \
 -o .artifacts/780/unlink/syscalls.log \
 target/debug/deps/shared_workspace_backend-9e4a9883436fe3ee \
 invalid_project_config_reports_error_and_keeps_last_good_shared_view --ignored --exact --nocapture \
 > .artifacts/780/unlink/run.log 2>&1
# Наблюдательный предел вернул 124; это НЕ успешное завершение теста.
cp --reflink=auto .artifacts/780/app.saved target/debug/bsl-analyzer-app
```

Наблюдения:

* `unlink/run.log:7`: `PROBE unlink result=0`.
* `unlink/syscalls.log:4145`: `2518171 readlink("/proc/self/exe", ".../target/debug/bsl-analyzer-app (deleted)", 256) = 107`.
* `unlink/syscalls.log:4225`: `2519566 execve(".../target/debug/bsl-analyzer-app (deleted)", [..., "mcp", "serve", "--profile", "workspace", "--mode", "daemon", ...], ...) = -1 ENOENT`.
* `unlink/stderr-2518171.log:41`: `ERROR LSP session failed error="LSP workspace rebind failed: No such file or directory (os error 2)"`.
* `unlink/syscalls.log:4321`: `2518171 +++ exited with 1 +++`.
* После этой строки тест продолжил открывать `shared-daemon.log`: **270** открытий до внешней остановки (команда подсчёта ниже).
* Сохранённый daemon log (`unlink/daemon.log:19,21,224-225`): один `session_attached` LSP, один `owner_init_start`; затем `shared workspace scope retired; closing broker endpoint` и `shared broker adapter session ended with error`. Это та же наблюдаемая форма отказа, что в issue.
* После пробы `uptime`: `17:44:04 ... load average: 77,30, 80,15, 59,91`.

Команда подсчёта (вывод `270`, затем `1 1`):

```sh
python3 - <<'PY'
from pathlib import Path
p = Path('.artifacts/780/unlink/syscalls.log').read_text().splitlines()
i = next(i for i, l in enumerate(p) if '2518171 +++ exited' in l)
print(sum('shared-daemon.log' in l for l in p[i+1:]))
s = Path('.artifacts/780/unlink/daemon.log').read_text().splitlines()
print(sum('owner_init_start' in l for l in s), sum('session_attached' in l for l in s))
PY
```

Daemon log скопирован из runtime, указанного в openat трассы: `/home/itrous/src/.temp/.tmpqGG3SO/shared-daemon.log`; при повторении путь определяется из новой трассы.

Причинная цепочка по исходникам: `crates/bsl-analyzer/src/bin/cli/lsp.rs:433-475` входит в reconnect только после `ScopeRetired`; `:360-372` использует `current_exe()` при запуске; `crates/bsl-analyzer/tests/shared_workspace_backend.rs:398-406` читает только лог и не имеет ветки завершения по EOF/ошибке адаптера. Поэтому ошибка запуска превращается в бесконечное ожидание второго маркера. Длительность 20 секунд не доказывает бесконечность — её доказывает отсутствие иной ветки выхода из цикла; трасса показывает, что выполнена именно эта ветка.

## Положительный контроль изоляции бинарника

Повторена та же проба и тот же тест, но исходный путь был symlink на отдельную копию. Это способ запустить **неизменённый** тест с зашитым `CARGO_BIN_EXE` на сохранённом inode, а не предлагаемая реализация.

```sh
mkdir -p .artifacts/780/stable
cp --reflink=auto target/debug/bsl-analyzer-app .artifacts/780/stable/bsl-analyzer-app
rm target/debug/bsl-analyzer-app
ln -s "$PWD/.artifacts/780/stable/bsl-analyzer-app" target/debug/bsl-analyzer-app
PROBE_OUTPUT="$PWD/.artifacts/780/stable" \
 PROBE_UNLINK="$PWD/target/debug/bsl-analyzer-app" \
 LD_PRELOAD="$PWD/.artifacts/780/probe.so" \
 timeout 30 strace -f -s 256 -e trace=execve,readlink,openat,clock_nanosleep \
 -o .artifacts/780/stable/syscalls.log \
 target/debug/deps/shared_workspace_backend-9e4a9883436fe3ee \
 invalid_project_config_reports_error_and_keeps_last_good_shared_view --ignored --exact --nocapture \
 > .artifacts/780/stable/run.log 2>&1
if test -L target/debug/bsl-analyzer-app; then rm target/debug/bsl-analyzer-app; fi
cp --reflink=auto .artifacts/780/app.saved target/debug/bsl-analyzer-app
```

Вывод `stable/run.log:7,9,12`:

```text
shared backend test: attach_order=s19-invalid-config-last-good phase=PROBE unlink result=0
shared backend test: attach_order=s19-invalid-config-last-good phase=valid repair attached the preserved LSP session to a new owner epoch
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 12.66s
```

Exit 0. `uptime`: `17:44:51 ... load average: 51,75, 73,13, 58,50`.
`sha256sum target/debug/bsl-analyzer-app .artifacts/780/app.saved` после восстановления дал одинаковый `4c38602d1de4e4b16b41d6f8a5f52e06cb232ac0ac5c374a820aaefe2791bda0`.
Положительный контроль отличает запуск с сохранённого inode от запуска с удаляемого исходного inode; простое наличие второй копии без её использования этого результата не даёт.

## Измерительный инструмент

Сохранить следующий текст как `.artifacts/780/probe.c`; собрать `cc -shared -fPIC -o .artifacts/780/probe.so .artifacts/780/probe.c -ldl` (наблюдён exit 0). Перед каждым запуском создать каталог PROBE_OUTPUT. Он сохраняет stderr каждого процесса, не забирая его у существующего читателя теста и не меняя BSL_LOG_FILE/маршрут daemon markers.

```c
#define _GNU_SOURCE
#include <dlfcn.h>
#include <unistd.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <fcntl.h>
#include <sys/syscall.h>
ssize_t write(int fd, const void *buf, size_t size) {
    static ssize_t (*real_write)(int, const void *, size_t);
    if (!real_write) real_write = dlsym(RTLD_NEXT, "write");
    if (fd == 2) {
        const char *dir = getenv("PROBE_OUTPUT");
        if (dir) {
            char path[4096];
            snprintf(path, sizeof(path), "%s/stderr-%d.log", dir, getpid());
            int log = open(path, O_WRONLY | O_APPEND | O_CREAT, 0600);
            if (log >= 0) { syscall(SYS_write, log, buf, size); close(log); }
        }
        const char *exe = getenv("PROBE_UNLINK");
        const char *phase = "frozen last-good owner still answers definition and hover";
        if (exe && memmem(buf, size, phase, strlen(phase))) {
            int result = unlink(exe);
            char note[128];
            int len = snprintf(note, sizeof(note), "PROBE unlink result=%d\n", result);
            real_write(2, note, len);
        }
    }
    return real_write(fd, buf, size);
}
```

## Соседние контроли на неизменённой базе

Запущены команды ниже (первые две с тем же LD_PRELOAD без PROBE_UNLINK, наблюдательный предел `timeout 900`); каждая вернула exit 0:

```sh
cargo test -p bsl-analyzer --test shared_workspace_backend scope_rebind_rechecks_open_buffers_and_preserves_public_lsp_state -- --ignored --exact --nocapture
cargo test -p bsl-analyzer --test shared_workspace_backend dotenv_configuration_root_change_rebinds_shared_lsp_and_retires_mcp -- --ignored --exact --nocapture
cargo test -p bsl-analyzer --test shared_workspace_analysis_s10 invalid_project_config_preserves_lsp_and_rejects_mcp_until_repair -- --exact --nocapture
```

`rg -n 'test result:' .artifacts/780/neighbors/*.log .artifacts/780/s10.log`:

```text
scope_rebind_rechecks_open_buffers_and_preserves_public_lsp_state.log:14:test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 2.88s
dotenv_configuration_root_change_rebinds_shared_lsp_and_retires_mcp.log:11:test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 1.17s
s10.log:10:test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 2 filtered out; finished in 1.80s
```

Это исходные положительные контроли, а не проверка ещё не написанного исправления.

## Граница вывода

Проверен механизм `unlink → current_exe deleted → ENOENT → ошибка reconnect → завершение адаптера → ожидание только маркера`. Не проверено, что исторический отказ #780 вызван именно параллельным cargo: исходного stderr нет. Детерминированный вход заменяет невоспроизводимую историческую гонку согласно постановке; это не утверждение о поведении cargo. Неприоритетный select не потерял ScopeRetired в этом отказе — иначе ветка reconnect с данным сообщением ошибки не исполнилась бы. Продуктовая гарантия переживать удаление собственного executable не добавляется.
