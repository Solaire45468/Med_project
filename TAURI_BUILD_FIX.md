# Tauri Android build fix — LocalNote 0.3.4

If the build prints:

`Permission opener:default not found`

the project contains a stale permission from an older configuration. This project does not use
`tauri-plugin-opener`, so `opener:default` must not be present.

The included `build.py` removes the stale JSON capability entry automatically and clears
`src-tauri/target` only when it actually changed the capability files.

Manual fix:

```bash
grep -RIn 'opener:default' src-tauri/capabilities
rm -rf src-tauri/target
cargo tauri android build --target aarch64
```

Do not add `opener:default` back unless `tauri-plugin-opener` is installed and its permissions
are intentionally configured.


## If your existing checkout still says v0.3.0

Run from the project root:

```bash
python3 fix_tauri_permissions.py
rm -rf src-tauri/target
cargo tauri android build --target aarch64
```

The repair script scans `src-tauri/capabilities` and the rest of the source tree (excluding generated `target` data), removes stale `opener:default` JSON capability entries, and refuses to silently rewrite TOML permissions.

## После исправления

Ожидаемая версия Cargo-пакета: `med-app v0.3.4`. Если вывод всё ещё показывает `v0.3.0`, собирается старая копия проекта.
