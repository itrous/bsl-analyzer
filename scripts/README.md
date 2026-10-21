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
python3 -m unittest discover -s scripts/perf-nightly/tests
```

Сценарии: `analyze`, `lsp-session`, `lsp-workspace-diag`, `mcp-memory`, `mcp-start`,
`mcp-load`. Warm-серия сценария идёт раньше cold, `mcp-memory` — раньше `mcp-start`
и `mcp-load` той же серии. Если на машине работает чужой `bsl-analyzer*`, прогоны
отменяются с `foreign_process`.
