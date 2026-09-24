# Documentation check — LocalNote 0.3.4

Verified against current Tauri 2 / Rust documentation on 2026-09-24:

- Tauri global `window.__TAURI__.core.invoke` is valid when `app.withGlobalTauri` is true.
- Tauri custom commands belong in `invoke_handler(tauri::generate_handler![...])`; commands defined in `lib.rs` should not be public.
- Capabilities grant access to specific commands and may be stored in `src-tauri/capabilities`.
- `opener:default` is an opener-plugin permission and is not valid when `tauri-plugin-opener` is not a dependency.
- Rust byte string literals `b"..."` are ASCII-only; UTF-8 test input uses `"...".as_bytes()`.
- Rust `?` converts errors through `From`; command functions returning `Result<_, String>` must explicitly map `AppError` to `String`.
- Reqwest uses system proxy settings by default; this app explicitly disables proxies, disables redirects, and sets bounded timeouts for local AI traffic.
- Android WebView permission handling for microphone is supported by current Tauri 2 webview APIs; the Android manifest includes `RECORD_AUDIO`.

The project was statically checked in this environment. A full Android build still requires the user's local Rust/Android toolchain.


Additional verified points for 0.3.4:
- `app.windows[].useHttpsScheme` is a documented Tauri 2 setting for `https://tauri.localhost` on Android/Windows; AI requests are backend-only, so the frontend no longer requires HTTP loopback access in CSP.
- `window` labels default to `main`; the project now declares the label explicitly to match the capability.
- `reqwest::ClientBuilder::no_proxy()` explicitly disables system proxy use; `Policy::none()` disables redirects.
- Android build helper adds `RECORD_AUDIO`, `INTERNET`, and `MODIFY_AUDIO_SETTINGS`, plus an optional microphone hardware feature declaration.
