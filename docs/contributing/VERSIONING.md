# Политика версионирования

## Схема версий

bsl-analyzer использует [SemVer](https://semver.org/) в формате `0.MINOR.PATCH`:

```
0.3.0  — релиз с ломающим изменением (minor растёт)
0.3.1  — следующий релиз без ломающих изменений (patch растёт)
```

Пока проект в фазе `0.x`, breaking changes возможны в любом релизе; релиз с
ломающим изменением повышает minor и помечается в `CHANGELOG.md` разделом
`BREAKING`.

### Внутренние крейты

Внутренние крейты не являются публичным API проекта и могут версионироваться
независимо от CLI-бинаря. Источником истины для пользовательского релиза служит
`[workspace.package].version` в корневом `Cargo.toml`.

## Теги Git

Формат: `v0.MINOR.PATCH`

```bash
# Создание тега
git tag v0.<minor>.<patch>
git push origin v0.<minor>.<patch>
```

Версии только идут вперёд. Нельзя переиспользовать или перемещать тег на другой коммит.

## Процесс релиза

1. Зафиксировать все изменения
2. Обновить `version` в `Cargo.toml` (`[workspace.package]`)
3. Включить обновлённый `Cargo.lock` в коммит
4. Создать тег и запушить

```bash
# Пример
cargo build --release  # проверить сборку
git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to 0.<minor>.<patch>"
git tag v0.<minor>.<patch>
git push origin develop --tags
```

## Зависимости

- **Patch-версии** — обновлять свободно
- **Minor-версии** — проверить changelog
- **Major-версии** — тестировать, обновлять с осторожностью

Критичные зависимости закрепляются:

```toml
[workspace.dependencies]
rowan = "=0.16.1"       # Закреплено
salsa = "0.26"          # Разрешены patch-обновления
```
