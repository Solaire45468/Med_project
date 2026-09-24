# LocalNote 0.3.4 — verification checklist

Static checks performed in the build workspace on 2026-09-24:

- `node --check src/ui/js/main.js`
- `node --check src/recorder-worklet.js`
- `python3 -m py_compile build.py fix_tauri_permissions.py`
- JSON parse of `src-tauri/tauri.conf.json` and `src-tauri/capabilities/main.json`
- all local HTML/CSS/JS/worklet references resolve to existing files
- no `opener:default` remains in `src-tauri`
- no Cyrillic byte-string literals (`b"..."`) remain in Rust source
- every frontend `invoke(...)` command is registered in `generate_handler!`
- no remote `<script src="http...">` is present in the frontend

Not performed in this container:

- `cargo check`
- `cargo test`
- `cargo tauri android build`

The current environment has no Rust/Cargo executable, so those checks must be run in the Android/Rust development environment.
