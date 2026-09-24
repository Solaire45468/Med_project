import json
import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path


def check_and_get_env():
    env_path = Path(".env")
    if not env_path.exists():
        print("❌ Файл .env не найден.")
        print("Создайте его только если нужна автоматическая подпись APK.")
        return {}

    env_vars = {}
    for line in env_path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            env_vars[key.strip()] = value.strip().strip("\"'")
    return env_vars


def run_cmd(args, env_override=None):
    printable = " ".join(shlex.quote(str(x)) for x in args)
    print(f"\n→ {printable}")

    # Объединяем текущие системные переменные с переданными переопределениями
    cmd_env = os.environ.copy()
    if env_override:
        cmd_env.update(env_override)

    result = subprocess.run(args, env=cmd_env)
    if result.returncode != 0:
        raise SystemExit(f"❌ Команда завершилась с кодом {result.returncode}")


def clean_stale_capabilities():
    """Remove stale opener permissions from source configuration only."""
    src = Path("src-tauri")
    target = src / "target"
    bad = "opener:default"
    changed = False
    offenders = []

    for path in src.rglob("*"):
        if (
            not path.is_file()
            or target in path.parents
            or ".git" in path.parts
            or "gen" in path.parts
        ):
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if bad not in text:
            continue
        offenders.append(path)
        if path.suffix.lower() in {".json", ".json5"}:
            try:
                data = json.loads(text)
            except json.JSONDecodeError as exc:
                raise SystemExit(f"❌ Некорректный JSON в {path}: {exc}")
            if isinstance(data, dict) and isinstance(data.get("permissions"), list):
                new_permissions = [item for item in data["permissions"] if item != bad]
                if new_permissions != data["permissions"]:
                    data["permissions"] = new_permissions
                    path.write_text(
                        json.dumps(data, ensure_ascii=False, indent=2) + "\n",
                        encoding="utf-8",
                    )
                    changed = True
        else:
            raise SystemExit(f"❌ {bad} найден в неподдерживаемом конфиге: {path}")

    if offenders:
        print("Найдены старые ссылки opener:default:")
        for path in sorted(set(offenders)):
            print(f"  {path}")
    else:
        print("✅ opener:default в исходной конфигурации не найден.")

    if changed and target.exists():
        print("🧹 Очищаю старый src-tauri/target после изменения permissions...")
        shutil.rmtree(target)

    return changed


def patch_manifest():
    path = Path("src-tauri/gen/android/app/src/main/AndroidManifest.xml")
    if not path.exists():
        raise SystemExit(f"❌ Манифест не найден: {path}")

    content = path.read_text(encoding="utf-8")
    permissions = [
        '<uses-permission android:name="android.permission.RECORD_AUDIO" />',
        '<uses-permission android:name="android.permission.INTERNET" />',
        '<uses-permission android:name="android.permission.MODIFY_AUDIO_SETTINGS" />',
        '<uses-feature android:name="android.hardware.microphone" android:required="false" />',
    ]

    missing = [perm for perm in permissions if perm not in content]
    if missing:
        marker = "<application"
        if marker not in content:
            raise SystemExit("❌ В AndroidManifest.xml не найден тег <application>.")
        content = content.replace(marker, "\n".join(missing) + "\n\n    " + marker, 1)
        path.write_text(content, encoding="utf-8")
        print("✅ Добавлены RECORD_AUDIO и INTERNET.")
    else:
        print("ℹ️ Android permissions уже присутствуют.")


def sign_apk(apk_path, env):
    keystore = env.get("KEYSTORE_PATH")
    alias = env.get("KEYSTORE_ALIAS")
    password = env.get("KEYSTORE_PASSWORD")
    if not all([keystore, alias, password]) or "твой_пароль" in password:
        print("ℹ️ Автоподпись отключена: параметры .env не заданы.")
        return apk_path

    keystore_file = Path(keystore).expanduser()
    if not keystore_file.exists():
        raise SystemExit(f"❌ Keystore не найден: {keystore_file}")

    android_home = Path(os.environ.get("ANDROID_HOME", Path.home() / "Android/Sdk"))
    versions = sorted((android_home / "build-tools").glob("*"), reverse=True)
    apksigner = None
    for version in versions:
        candidate = version / "apksigner"
        if candidate.exists():
            apksigner = candidate
            break

    if not apksigner:
        print("⚠️ apksigner не найден; APK останется неподписанным вашим ключом.")
        return apk_path

    signed = apk_path.with_name(apk_path.stem + "-signed.apk")
    run_cmd(
        [
            str(apksigner),
            "sign",
            "--ks",
            str(keystore_file),
            "--ks-pass",
            f"pass:{password}",
            "--out",
            str(signed),
            str(apk_path),
        ]
    )
    return signed


def main():
    print("=========================================")
    print(" LocalNote — сборка Tauri Android")
    print("=========================================")

    env = check_and_get_env()

    clean_stale_capabilities()
    run_cmd([sys.executable, "fix_tauri_permissions.py"])

    android_dir = Path("src-tauri/gen/android")
    if not android_dir.exists():
        run_cmd(["cargo", "tauri", "android", "init"])

    patch_manifest()

    # Автоматически определяем Android SDK и формируем BINDGEN_EXTRA_CLANG_ARGS с API 30
    android_home = Path(os.environ.get("ANDROID_HOME", Path.home() / "Android/Sdk"))
    ndk_path = android_home / "ndk/29.0.13846066"
    sysroot_path = ndk_path / "toolchains/llvm/prebuilt/linux-x86_64/sysroot"

    api_level = "30"
    clang_args = f"--target=aarch64-linux-android --sysroot={sysroot_path} -D__ANDROID_API__={api_level}"

    print(
        f"🔧 Настраиваем NDK Sysroot и API {api_level} для сборки биндингов: {sysroot_path}"
    )

    # Передаем переменные окружения для корректной сборки C/C++ кода в llama.cpp под Android API 30
    build_env = {
        "BINDGEN_EXTRA_CLANG_ARGS": clang_args,
        "BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android": clang_args,
        "CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS": "-C link-arg=-landroid -C link-arg=-llog",
        "ANDROID_API_LEVEL": api_level,
        "CC_aarch64_linux_android": str(
            ndk_path
            / f"toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android{api_level}-clang"
        ),
        "CXX_aarch64_linux_android": str(
            ndk_path
            / f"toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android{api_level}-clang++"
        ),
    }

    run_cmd(
        ["cargo", "tauri", "android", "build", "--target", "aarch64"],
        env_override=build_env,
    )

    apk_root = Path("src-tauri/gen/android/app/build/outputs/apk")
    apks = sorted(apk_root.rglob("*.apk")) if apk_root.exists() else []
    if not apks:
        raise SystemExit(f"❌ Release APK не найден в {apk_root}.")
    apks.sort(key=lambda p: ("release" not in str(p).lower(), str(p)))

    apk = sign_apk(apks[0], env)
    print(f"\n✅ APK: {apk}")

    adb = subprocess.run(["adb", "devices"], capture_output=True, text=True)
    devices = [
        line
        for line in adb.stdout.splitlines()[1:]
        if line.strip() and "device" in line
    ]
    if devices:
        run_cmd(["adb", "install", "-r", str(apk)])
        print("✅ APK установлен на подключённое устройство.")
    else:
        print("ℹ️ ADB-устройство не обнаружено — установка пропущена.")


if __name__ == "__main__":
    main()
