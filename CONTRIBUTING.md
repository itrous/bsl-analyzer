# Участие в разработке `bsl-analyzer`

Спасибо за интерес к проекту. Этот документ служит точкой входа для
контрибьюторов: здесь собраны ожидания по процессу, а профильные правила вынесены
в отдельные документы.

## С чего начать

Минимальное окружение:

- Rust 1.95+
- Git

Базовая подготовка репозитория:

```bash
git clone https://github.com/itrous/bsl-analyzer.git
cd bsl-analyzer
./scripts/setup-hooks.sh
```

После клонирования убедитесь, что проект собирается и тесты проходят:

```bash
cargo build
cargo test --all
```

Полный список локальных проверок и правила по качеству кода описаны в
`docs/contributing/DEVELOPMENT_RULES.md`.

## Куда смотреть дальше

- `docs/README.md` — карта документации проекта
- `docs/contributing/DEVELOPMENT_RULES.md` — код-стайл, тесты, правила по диагностике и производительности
- `docs/contributing/LOGGING.md` — `tracing`, profiling и переменные окружения
- `docs/contributing/VERSIONING.md` — релизы, теги и обновление версий
- `docs/contributing/SALSA_GUIDE.md` — практические заметки по Salsa
- `docs/architecture/ARCHITECTURE.md` — обзор слоёв и основных пайплайнов

## Рабочий цикл контрибуции

1. Создайте ветку от `develop`.
2. Если изменение заметное по масштабу, сначала зафиксируйте задачу в issue или
   опишите её в Merge Request.
3. Вносите изменения вместе с тестами и обновлением релевантной документации.
4. Перед публикацией прогоните локальные проверки.
5. Откройте Merge Request с кратким описанием, ссылками на связанные задачи и
   перечислением затронутых сценариев.

Типичная последовательность команд:

```bash
git checkout -b feature/my-change
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
git push -u origin feature/my-change
```

## Что считается хорошим вкладом

- изменение сопровождается тестами или понятным объяснением, почему тест не нужен;
- документация обновляется вместе с кодом, если меняется поведение CLI, конфигов,
  диагностик или архитектурных ограничений;
- новые diagnostics оформлены полностью: код обработчика, metadata, тесты и
  справка в `crates/ide-diagnostics/docs/`;
- локальные проверки проходят без warnings и случайного отладочного вывода.

Подробные правила и примеры остаются в
`docs/contributing/DEVELOPMENT_RULES.md`, чтобы не дублировать их здесь.

## Коммиты и Merge Request

Для сообщений коммитов используем
[Conventional Commits](https://www.conventionalcommits.org/):
`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:` и т.д.

Для Merge Request важно:

- понятно описать, что изменилось и зачем;
- указать влияние на пользователей, конфигурацию и документацию;
- приложить ссылки на issue или обсуждение, если они есть.

Политика версий, релизные шаги и правила работы с тегами вынесены в
`docs/contributing/VERSIONING.md`.

## Если вы меняете диагностики

Минимальный маршрут почти всегда такой:

1. добавить или обновить обработчик в `crates/ide-diagnostics/src/handlers/`;
2. обновить metadata и регистрацию кода диагностики;
3. добавить тесты;
4. обновить справку в `crates/ide-diagnostics/docs/`;
5. проверить результат через `bsl-analyzer rules list` или профильные тесты.

За детальными шаблонами и критериями выбора между AST/HIR/CFG/Dataflow/SDBL
смотрите `docs/contributing/DEVELOPMENT_RULES.md`.

## Вопросы и лицензия

- Issues: https://github.com/itrous/bsl-analyzer/issues
- Pull Requests: https://github.com/itrous/bsl-analyzer/pulls

Отправляя изменения в репозиторий, вы соглашаетесь, что ваш вклад
распространяется на условиях лицензии MIT (см. `LICENSE-MIT`), если явно не
согласовано иное.
