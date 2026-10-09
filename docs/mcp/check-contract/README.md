# Контракт внешней проверки BSL, версия 1

Крейт `check-contract` содержит общие JSON-типы и проверки. Он не зависит от
MCP-сервера, исполнителя или платформы 1С. JSON Schema генерируются из тех же
serde-типов и лежат в [`v1/`](v1/).

Документ задаёт интерфейс будущей интеграции. Загрузчик `check_executor`,
MCP-адаптер и нативный исполнитель добавляются отдельными изменениями;
этот крейт сам не запускает процессы и не проверяет BSL.

Повторная генерация:

```sh
cargo run -p check-contract --example generate_schemas
```

## Профиль `connections`

Файл профилей сохраняет корневой объект `connections`. Пример задаёт только
демонстрационную конфигурацию: имя профиля и локальные пути не содержат
секретов. URL отсутствует, поэтому значения HTTP-переменных окружения для
этого профиля не требуются.

```json
{
  "connections": {
    "demo": {
      "check_executor": {
        "program": "/usr/local/bin/bsl-native-check",
        "args": ["--config", "/etc/bsl-check-executor/profiles.json", "--profile", "demo"],
        "timeout_ms": 120000,
        "max_code_bytes": 2097152,
        "max_response_bytes": 262144,
        "max_stderr_bytes": 65536,
        "stop_grace_ms": 2000,
        "reap_timeout_ms": 1000,
        "max_concurrent": 1,
        "handles_snippet": true
      }
    }
  }
}
```

`program` и `args` доверенные и фиксированные; caller запускает их без shell.
`url` необязателен при наличии `check_executor`. HTTP-переменные учётных данных
нужны только профилю с HTTP URL. Без URL профиль не должен требовать эти
переменные; HTTP-операции `run`, `eval` и `event_log` остаются недоступны.

`handles_snippet` обязателен и явно выбирает маршрут. При `true` snippets идут
исполнителю. При `false` snippets идут по HTTP, если задан URL, иначе получают
`unsupported`. Module всегда идёт исполнителю. Ошибка выбранного backend не
переключает запрос на другой маршрут.

## Лимиты и остановка

Каждый запрос передаёт пять обязательных положительных полей:
`timeout_ms`, `max_code_bytes`, `max_response_bytes`, `max_stderr_bytes` и
`stop_grace_ms`. У исполнителя нет собственных defaults для этих wire-полей.
Caller проверяет минимумы профиля: 1000 ms для timeout, 16384 байта для ответа
и 500 ms для grace. Значения caller по умолчанию: 120000 ms, 2097152 байта
кода, 262144 байта ответа, 65536 байт stderr и 2000 ms grace. Размер кода
считается в UTF-8 байтах; frame ограничен выражением
`6 * max_code_bytes + 64 KiB` с проверкой переполнения.

`reap_timeout_ms` относится только к caller и не входит в JSON-запрос. Его
значение по умолчанию — 1000 ms. Ноль, неверный тип и значение, которое нельзя
представить используемым timer, отвергаются до запуска процесса. Это окно
ожидания reap после hard-stop; оно не меняет `stop_grace_ms`.

## Ответ и координаты

Все фиксированные nullable-поля ответа должны присутствовать, даже когда их
значение неизвестно: в этом случае передаётся `null`. Это правило действует
и во вложенных объектах `compiler` и `issues`. Неизвестные поля запрещены во
всех contract-объектах, кроме открытого объекта `backend_info`. У ответа нет
верхнеуровневого `owner`: metadata owner находится в `context.owner` вместе с
origin конфигурации или расширения.

Координаты issue относятся к исходному тексту: line и column начинаются с 1,
column измеряется в UTF-16 code units. Позиция — либо пара положительных чисел,
либо два явных `null`. Начальный BOM не сдвигает позицию; CRLF считается одним
переводом строки, tab — одним code unit, символ вне BMP — двумя.

Если HTTP-сервис вернул отрицательный результат без доказуемой исходной
позиции, caller сохраняет текст ошибки отдельно и формирует общий ответ со
`status: "error"`, `failure.code: "position_unavailable"`,
`compilation_status: "invalid"`, `contexts_checked: ["server"]` и issue с
`line: null`, `column: null`. Caller не выдумывает координаты.

## Проверки схемы и runtime

JSON Schema проверяют форму данных: обязательные поля, nullable-значения,
закрытые enum, запрещённые лишние поля и положительный диапазон wire-лимитов.
Дрейф сохранённых схем ловит тест `schemas_match_the_types_and_require_nullable_fields`.
Schema не выражает все связи между полями: caller дополнительно проверяет
соответствие `status` и `valid`, покрытие обязательных compiler contexts,
координаты, request id и connection, совпадение module type и metadata
owner/origin, а также запрещённые caller failure-коды.

Сам контракт не может доказать код завершения процесса: например, ответ с
`valid: true` при exit status 1 должен быть отвергнут caller как
`transport_exit_status`. Это проверяется fake-executor suite caller-а на
следующем этапе; fixture PR1 проверяют только JSON и правила контракта.
