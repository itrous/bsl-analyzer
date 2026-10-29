# Scripts

Вспомогательные скрипты для разработки bsl-analyzer.

## setup-hooks.sh

Установка git pre-commit hooks.

```bash
./scripts/setup-hooks.sh
```

Устанавливает pre-commit hook в `.git/hooks/`, который автоматически запускает перед каждым коммитом:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets --all-features -- -D warnings`

## release.sh

Локальная сборка релиза.

```bash
./scripts/release.sh 0.1.37
```

- Собирает `bsl-analyzer` в release mode
- Определяет платформу (linux/darwin/windows, amd64/arm64)
- Вычисляет SHA256 checksum

## perf-nightly/

Замеры LSP, MCP и `analyze` на приватных корпусах (`docs/plans/perf-nightly.md`).
Только стандартная библиотека Python. Каталог данных — вне репозитория: харнес
отказывается работать с `--data-dir` внутри рабочего дерева.

```bash
D=~/.pio/jobs/perf-nightly/data                 # corpora.toml лежит здесь
P=scripts/perf-nightly/perf_nightly.py
python3 $P build --data-dir $D --ref v0.3.2     # → $D/bin/<label>-<sha12>-<rustc12>/
python3 $P snapshot --data-dir $D --corpus crm
python3 $P points --data-dir $D --corpus crm --binary $B
python3 $P helpcheck --data-dir $D --binary $B
python3 $P measure --data-dir $D --corpus crm --binary $B \
  --scenario lsp-session --temp warm --series s1 --runs 3
python3 $P report --data-dir $D --corpus crm --series s1
python3 $P suite --data-dir $D --binary $B --series night-2026-10-21  # ночной набор одного бинаря
python3 $P calibrate --data-dir $D --binary $B --series 5   # cal-<i> и $D/noise.json
python3 $P compare --data-dir $D --base v0.3.2/cal-1 --candidate v0.3.2/cal-2 --trend
python3 $P night --data-dir $D --deadline 08:45            # → $D/nights/<дата>/
python3 $P night --data-dir $D --candidate v0.3.3          # гейт тега → $D/gates/v0.3.3/
python3 $P gate-calibrate --data-dir $D --base v0.4.1      # → $D/gate-cal/…, $D/gate-noise.json
python3 $P gate --data-dir $D --base v0.4.1 --candidate <SHA|тег> --label <метка>  # → $D/gates/<метка>/
python3 -m unittest discover -s scripts/perf-nightly/tests
```

`night` собирает develop (или тег-кандидат) и последний релиз-предок, меряет ночной
набор обоих в одном окне и пишет `summary.json`, `report.md` и `compare.json` с
вердиктом `ok`, `issue`, `block` или `incomplete`. Прогон, который не успевает к
дедлайну по оценке прошлой ночи, пропускается с `window_exhausted`, и ночь не может
быть `ok`. Без актуального `noise.json` ночь вместо набора меряет две серии
калибровки последнего релиза, а после пятой пишет `noise.json`. Калибровку и
сравнимость сбрасывают смена CPU, числа потоков, снимка корпуса, его режима, файла
точек или схемы харнеса; версии ядра и `rustc` и объём памяти пишутся в каждый
прогон, но обновление системы калибровку не сбрасывает. Положительные
контроли `measure`: `--ballast-mb`, `--ballast-after-ready-mb`, `--inject-latency-ms`;
контроль времени — запуск `measure` под `taskset -c 0-3`.

`gate` — гейт скорости и памяти ночного релиза. Меряет на клоне ЕРП из `corpora.toml`
(на коммите манифеста, без изменённых отслеживаемых файлов, иначе `source_mismatch`) с
его закоммиченным конфигом, свежим `BSL_CACHE_DIR` на прогон и порядком вызовов и пауз
прежнего протокола релиза. Серия A — три пары база/кандидат с чередованием: время
`graph_ready_s` от запуска до первого `graph ready`, CPU прогона к этому моменту
`graph_ready_cpu_s`, затем `graph resolve` (`graph_resolve_s`) и свип `diagnostics
workspace` на 5000 файлов (`diagnostics_workspace_s`); серия B — кандидат ×3 с
`--enable-tool references`. Δ — медиана кандидата к медиане базы минус 1, ν — шум из
`$D/gate-noise.json`:

| метрика | исход |
|---|---|
| `graph_ready_cpu_s`, `graph_resolve_s`, `diagnostics_workspace_s` | ν нет — `no_noise`; ν > 5 % — `noisy`; Δ > 5 % — `block` |
| `graph_ready_s` | ν нет — `no_noise`; ν > 40 % — `noisy`; Δ ≤ max(5 %, ν) — не блок; выше при ν ≤ 5 % — `block`, при ν > 5 % — подтверждающий раунд из трёх пар и `block`, только если и его Δ′ > max(5 %, ν) |
| `hwm_kb` кандидата (обе серии), references серии B | больше 7168 МиБ, 67,7 / 32,3 / 59,5 с — `block` |
| ответы `graph resolve` и `aggregates` свипа каждой пары | байты различны — `block` (`answers_differ`); у первого прогона кандидата два разных непустых доп. resolve не получены — `answers_control` |
| сбой или невалидный прогон | кандидата — `block`; базы или недобор прогонов — `incomplete` |

Прогон, где чужая нагрузка за окно готовности графа больше двух ядер (`foreign_cores`
по `/proc/stat`), не засчитывается (`foreign_load`) и повторяется на том же месте;
перед каждым прогоном гейт ждёт MemAvailable ≥ 13 ГиБ и load1 ≤ 2. Ожидание и
незасчитанные прогоны делят бюджет `--busy-budget-min` (60 минут), его исчерпание —
`incomplete` с `machine_busy`. Вердикт `pass`, `block` или `incomplete` (коды выхода
0 / 1 / 2) — в `$D/gates/<метка>/summary.json` и `report.md`; занятые `gate.lock`
(`gate_busy`) и существующий каталог метки (`gate_exists`) — отказ без `summary.json`.
Калибровка гейта — `gate-calibrate`: пять серий по три полных прогона базы, отдельный
`gate-noise.json` (ночной `noise.json` не трогается); его устаревают смена CPU, числа
потоков, схемы харнеса или гейта, коммита ЕРП и его конфига, без него гейт `incomplete`
с `noise_stale`. Положительные контроли (в регламенте не используются):
`--control-burn-cpu-s X` (кандидат тратит на X CPU-секунд больше до готовности графа),
`--control-start-delay-s D` (сервер кандидата стартует на D с позже),
`--control-foreign-burn N` (N занятых процессов вне прогона на окне готовности);
`--ready-only` судит только готовность и вместо `pass` пишет `probe-ok`.

Корпус, чей закоммиченный `bsl-analyzer.toml` проверяемые версии отвергают, получает
в `corpora.toml` поле `config` — путь к приватной копии конфига в каталоге данных;
рабочий конфиг снимка строится от неё, репозиторий корпуса не меняется.

Сценарии: `analyze`, `lsp-session`, `lsp-close-memory`, `lsp-workspace-diag`,
`mcp-memory`, `mcp-start`, `mcp-load`. Warm-серия сценария идёт раньше cold,
`mcp-memory` — раньше `mcp-start` и `mcp-load` той же серии, `lsp-session` — раньше
`lsp-close-memory`.

`lsp-close-memory` (только warm) в одной сессии трижды открывает модули точек `F_rot`,
`F` и `F_small`, ждёт их диагностик, запрашивает символы, подсказки, hover и
completion, закрывает все документы и две минуты снимает RSS каждые 30 с. Первым
запросом после диагностик открытых модулей цикл шлёт глобальный `workspace/symbol`
с запросом точки `Q`; ошибка или пустой ответ делают цикл неудачным. `measure`
печатает по циклу время открытия, время глобального запроса, RSS после закрытия и
прирост. Issue, блока нет: прирост цикла 2 и далее над памятью после прошлого
закрытия больше 50 МБ (`rss_growth_max_kb`); RSS после закрытия любого цикла над
памятью готовности больше 300 МБ (`rss_over_ready_max_kb`); глобальный запрос циклов
2 и 3 дольше 0,2 с (`global_reopen_s`). Неудачный цикл оставляет величины без
значения. База для этих порогов не нужна, поэтому ночь меряет сценарий только у
кандидата, одним прогоном, на УТ, и пишет его время, лимиты и таблицу циклов в
раздел отчёта «Память после закрытия документов»; исход раздела — худшая из строк
сценария.

Блок по времени (`*_s`) — рост больше 40% и одновременно больше 20 мс; меньший рост
судит правило issue по шуму. Метрики памяти блокируют по 40%, как прежде. Issue —
рост от 10% и больше шума; у `analyze` на УТ и ЕРП (`wall_s`, `max_rss_kb`) — от 5% и
больше шума, ЦРМ и Ломбард остаются на 10%. Абсолютные лимиты судят кандидата одного,
без базы и шума, и дают только issue; прежние относительные строки тех же величин
остаются: `init_s_limit` — `initialize` `lsp-session` дольше 0,1 с;
`rss_ready_kb_limit` — RSS `mcp-memory` после готовности больше 0,5 ГБ, только на ЦРМ;
`rss_growth_kb_limit` — прирост `mcp-memory` за серию больше 0,6 ГБ. В строке лимита
колонка «база» показывает `≤ <лимит>`. Лимиты в калибровку шума не входят.

Раздел отчёта «Число диагностик»: число диагностик `analyze` кандидата на УТ
сравнивается с последней прошлой ночью, где оно записано (`summary.json`,
`diag_counts`); база показывается только для справки. Состояния: «изменилось»,
«без изменений», «нестабильно между прогонами», «нет прошлой записи», «нет
удержанного прогона кандидата». Вердикт ночи число диагностик не меняет.

Ручной `measure` отменяет прогоны с `foreign_process`, если
на машине работает чужой `bsl-analyzer*`. `night`, `calibrate` и `suite` чужие процессы
не проверяют и не ждут: их нагрузка входит в замеры и в калибровку шума.
Процесс своего прогона, переживший останов, проверяется всегда: пока он жив, следующие прогоны
не запускаются и пишутся с `survivor_running`.
