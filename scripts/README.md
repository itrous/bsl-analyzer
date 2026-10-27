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

Корпус, чей закоммиченный `bsl-analyzer.toml` проверяемые версии отвергают, получает
в `corpora.toml` поле `config` — путь к приватной копии конфига в каталоге данных;
рабочий конфиг снимка строится от неё, репозиторий корпуса не меняется.

Сценарии: `analyze`, `lsp-session`, `lsp-close-memory`, `lsp-workspace-diag`,
`mcp-memory`, `mcp-start`, `mcp-load`. Warm-серия сценария идёт раньше cold,
`mcp-memory` — раньше `mcp-start` и `mcp-load` той же серии, `lsp-session` — раньше
`lsp-close-memory`.

`lsp-close-memory` (только warm) в одной сессии трижды открывает модули точек `F_rot`,
`F` и `F_small`, ждёт их диагностик, запрашивает символы, подсказки, hover и
completion, закрывает все документы и две минуты снимает RSS каждые 30 с. `measure`
печатает по циклу время открытия, RSS после закрытия и прирост. Прирост цикла 2 и
далее над памятью после прошлого закрытия больше 50 МБ — issue, блока нет; база для
этого порога не нужна, поэтому ночь меряет сценарий только у кандидата, одним
прогоном, на УТ, и пишет его время в раздел отчёта «Память после закрытия
документов».

Блок по времени (`*_s`) — рост больше 40% и одновременно больше 20 мс; меньший рост
судит правило issue по шуму. Метрики памяти блокируют по 40%, как прежде. Ручной `measure` отменяет прогоны с `foreign_process`, если
на машине работает чужой `bsl-analyzer*`. `night`, `calibrate` и `suite` чужие процессы
не проверяют и не ждут: их нагрузка входит в замеры и в калибровку шума.
Процесс своего прогона, переживший останов, проверяется всегда: пока он жив, следующие прогоны
не запускаются и пишутся с `survivor_running`.
