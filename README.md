# onec-buddy-mcp

> Этот проект основан на [ROCTUP/1c-buddy](https://github.com/ROCTUP/1c-buddy) и лишь повторяет его функциональность на Rust. Идея и все заслуги принадлежат автору оригинального проекта.

Локальный MCP-сервер для работы с «1С:Напарником» из Codex, Cursor и Claude Desktop. Сервер работает через стандартный ввод-вывод, не открывает входящий порт и выпускается для трёх целей:

| Система | Архив | Цель Rust |
|---|---|---|
| Windows x86-64 | `onec-buddy-mcp-windows-x86_64.zip` | `x86_64-pc-windows-msvc` |
| Linux x86-64 | `onec-buddy-mcp-linux-x86_64.tar.gz` | `x86_64-unknown-linux-gnu` |
| macOS Apple Silicon | `onec-buddy-mcp-macos-aarch64.tar.gz` | `aarch64-apple-darwin` |

## Инструменты

| Инструмент | Назначение |
|---|---|
| `ask_1c_ai` | Ответить на общий вопрос о разработке на платформе «1С:Предприятие» |
| `explain_1c_syntax` | Объяснить элемент встроенного языка или платформы |
| `check_1c_code` | Проверить синтаксис или провести обзор кода |
| `modify_1c_code` | Изменить код по заданию |
| `search_1c_documentation` | Найти сведения в документации платформы |
| `search_its` | Найти материалы в базе знаний ИТС |
| `fetch_its` | Получить документ или раздел ИТС |
| `diff_1c_documentation_versions` | Сравнить документацию двух версий платформы |

## Быстрый старт с Codex

1. Скачайте архив для своей системы и распакуйте его в постоянный локальный каталог. В Windows исполняемый файл называется `onec-buddy-mcp.exe`, в Linux и macOS — `onec-buddy-mcp`.
2. В Linux и macOS разрешите выполнение файла:

   ```sh
   chmod 0755 /абсолютный/путь/onec-buddy-mcp
   ```

3. Перед запуском Codex задайте личный токен «1С:Напарника»:

   ```powershell
   # Windows
   setx.exe ONEC_AI_TOKEN "<ваш_токен>"
   ```

   ```sh
   # Linux или macOS: переменная действует в текущем терминале
   export ONEC_AI_TOKEN="<ваш_токен>"
   ```

4. Добавьте сервер в `~/.codex/config.toml`. Укажите настоящий абсолютный путь без переменных и сокращений:

   ```toml
   [mcp_servers.onec_buddy]
   command = "/абсолютный/путь/onec-buddy-mcp"
   env_vars = ['ONEC_AI_TOKEN']
   ```

   Примеры путей: `C:\tools\onec-buddy-mcp\onec-buddy-mcp.exe`, `/home/alice/.local/lib/onec-buddy-mcp/onec-buddy-mcp`, `/Users/alice/.local/lib/onec-buddy-mcp/onec-buddy-mcp`.

5. Полностью перезапустите Codex и проверьте наличие восьми инструментов.

Запросы и фрагменты пользовательского кода передаются внешней службе `https://code.1c.ai`.

Установка, проверка архива, настройка Cursor и Claude Desktop, схемы инструментов, режимы и ограничения описаны в [подробном руководстве](README_FULL.md).

## Лицензия

Код распространяется по лицензии [GNU Affero General Public License 3.0](LICENSE).
