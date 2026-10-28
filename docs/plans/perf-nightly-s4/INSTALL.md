# Установка ночного job perf-nightly (после вливания среза 4)

Выполняет владелец-оркестратор, не этап. `<DATA>` — `data_dir` задания pio:
`~/.pio/jobs/perf-nightly/data` (`supervisor.job_dir` + `data`).

`perf-nightly.toml` — файл задания по §10.1 плана без таблицы `[tasks]`: шаблон
`perf-nightly-triage` появится со срезом 3, а модуль среза 4 запусков не просит (`runs: []`).
Вернуть `[tasks]` вместе со срезом 3.

1. Код харнеса — клон на origin/develop:
   `git clone https://github.com/itrous/bsl-analyzer.git <DATA>/src`
   (`git -C <DATA>/src checkout --detach origin/develop` модуль делает сам перед каждой ночью).
2. Модуль: `ln -s <DATA>/src/scripts/perf-nightly/pio_module.py ~/.pio/modules/perf-nightly`.
3. Задание: записать `perf-nightly.toml` в `~/.pio/tasks/perf-nightly.toml` (`pio_write`).
   Проверить, что id задания `perf-nightly` не спорит с файлом задачи `~/.pio/tasks/perf-nightly.md`.
4. `gh auth status` от пользователя демона: публикация perf-issue идёт через `gh`.
5. Ручной прогон: `pio sched run perf-nightly`. Ожидаемо: `<DATA>/nights/<дата>/summary.json` с
   вердиктом, `report.md`, уведомление `<дата>:verdict`; повторный `pio sched run` в ту же дату —
   без замеров и без новых вызовов `gh`. Первый прогон отдаёт `<дата>:unfinished` за последний
   прежний каталог `nights/<дата>/` без `summary.json`, если такой есть.
6. Только после приёмки ручного прогона: `pio sched add ~/.pio/tasks/perf-nightly.toml`.
