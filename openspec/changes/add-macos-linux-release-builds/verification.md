# Результаты проверки

Дата проверки: 30 июля 2026 года.

## Пройдено

- `openspec validate add-macos-linux-release-builds --strict`: изменение OpenSpec корректно.
- `actionlint 1.7.7`: ошибок в `.github/workflows/release.yml` нет.
- `cargo test --locked --target x86_64-unknown-linux-gnu --bin onec-buddy-mcp --all-features`: 96 проверок пройдено.
- `cargo test --locked --target x86_64-unknown-linux-gnu --test release_workflow_contract`: 4 проверки договора пройдено.
- `rustfmt --edition 2021 --check tests/release_workflow_contract.rs`: новый договорный тест отформатирован.
- Выпускной файл Linux собран в Docker с Rust 1.97.1, распознан как ELF x86-64 GNU/Linux и успешно вывел версию `0.1.1`.
- Пробный архив `onec-buddy-mcp-linux-x86_64.tar.gz` содержит ровно `onec-buddy-mcp`, `README.md`, `README_FULL.md`, `LICENSE` и `.env.example`.
- SHA-256 собранного и извлечённого из пробного архива файла `onec-buddy-mcp` совпадает.
- `git diff --check`: ошибок пробельного оформления нет.
- `git status --short -- vendor`: изменений в примерах `vendor` нет.

## Ограничения местной среды

- Нативная проверка Windows не запущена: `rustc`, Cargo и rustup отсутствуют и в `PATH`, и в обычном каталоге `%USERPROFILE%\.cargo\bin`. Установка средств разработки отдельно не разрешалась. Существующее задание Windows сохранено в сценарии автоматизации.
- Перекрёстная проверка `aarch64-apple-darwin` из Linux дошла до `aws-lc-sys` и остановилась из-за отсутствия компилятора и комплекта разработки Apple: GNU-компилятор не поддерживает ключи `-arch` и `-mmacosx-version-min`. Полная сборка и испытания назначены настоящему исполнителю `macos-14`.

## Старые локальные проверки

Общий запуск `cargo test --locked --target x86_64-unknown-linux-gnu --all-features` после 96 успешно пройденных модульных проверок встретил исключённый из Git файл `tests/ci_contract.rs`. Он требует отсутствующие `deny.toml` и `.github/workflows/windows.yml`. Этот локальный файл не входит в чистую копию проекта и в рамках изменения не исправлялся.

Полная проверка `cargo fmt --all -- --check` также видит исключённый из Git файл `tests/documentation_contract.rs` с прежним отклонением форматирования. Пользовательские исключённые файлы не изменялись.
