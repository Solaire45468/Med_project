# Security model

LocalNote is designed for a local-only workflow.

## Guarantees implemented in code

- AI endpoints are accepted only for `localhost`, `127.0.0.1`, `::1` or Android emulator host alias `10.0.2.2`.
- Reqwest proxies are disabled.
- HTTP redirects are disabled.
- CSP blocks external scripts, images, styles and network connections.
- Original transcript and summaries are encrypted with AES-256-GCM.
- The database key is derived with Argon2id from the user password and kept in memory only.
- Raw microphone PCM is kept in RAM and is not stored in SQLite.
- On lock/clear, the in-memory key/audio buffers are best-effort wiped.
- Tauri ACL exposes only the LocalNote commands to the main window.

## Threat-model limits

No application can guarantee that data cannot leak from a fully compromised operating system, rooted Android device, malicious local process, debugger, or a compromised local AI server. A loopback model server is trusted to process the text/audio it receives.

For medical use, the generated summary is an assistive representation, not a clinical diagnosis. The original transcript is retained so a person can verify the summary.

## Анализ содержимого

Локальная LLM получает только расшифровку текущей записи и контекст, введённый пользователем. Область не фиксируется приложением: модель определяет её по тексту. Полная оригинальная расшифровка и сгенерированный конспект хранятся отдельно внутри одного зашифрованного payload.
