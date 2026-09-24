use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use argon2::Argon2;
use rand::{rngs::OsRng, RngCore};
use reqwest::{multipart, redirect::Policy, Client};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{Manager, State};
use thiserror::Error;
use url::Url;
use zeroize::{Zeroize, Zeroizing};
use std::sync::Arc;
use llama_rs::{
    Model, // <--- Подключаем трейт Model, чтобы работал метод forward
    gguf::GgufFile,
    model::{load_llama_model, InferenceContext},
    sampling::{Sampler, SamplerConfig},
    tokenizer::Tokenizer,
};

const DB_FILE: &str = "vault.sqlite3";
const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const MAX_AUDIO_BYTES: usize = 80 * 1024 * 1024;
const KEY_CHECK: &[u8] = b"LocalNote key check v2";
const MAX_SETTING_URL_LEN: usize = 512;
const MAX_SETTING_MODEL_LEN: usize = 128;
const MAX_SUBJECT_LEN: usize = 160;
const MAX_TRANSCRIPT_LEN: usize = 2 * 1024 * 1024;

#[derive(Debug, Error)]
enum AppError {
    #[error("База данных не инициализирована")]
    NotInitialized,
    #[error("Неверный пароль или повреждённые данные")]
    InvalidPassword,
    #[error("База данных заблокирована")]
    Locked,
    #[error("Локальный адрес модели не разрешён: {0}")]
    UnsafeEndpoint(String),
    #[error("Ошибка БД: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("Ошибка сети локальной модели: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Ошибка JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Ошибка шифрования")]
    Crypto,
    #[error("{0}")]
    Message(String),
}

type AppResult<T> = Result<T, AppError>;

#[derive(Default)]
struct AppState {
    audio: Mutex<AudioBuffer>,
    key: Mutex<Option<Zeroizing<[u8; KEY_LEN]>>>,
}

#[derive(Default)]
struct AudioBuffer {
    pcm: Vec<u8>,
    sample_rate: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct NeuralSettings {
    stt_url: String,
    stt_model: String,
    llm_url: String,
    llm_model: String,
}

impl Default for NeuralSettings {
    fn default() -> Self {
        Self {
            // Контракт: OpenAI-compatible локальный endpoint, совместимый с
            // /v1/audio/transcriptions. Сервер должен работать на этом же устройстве.
            stt_url: "http://127.0.0.1:8000/v1/audio/transcriptions".into(),
            stt_model: "whisper".into(),
            // Ollama-compatible local API.
            llm_url: "http://127.0.0.1:11434/api/chat".into(),
            llm_model: "qwen2.5:7b".into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionPayload {
    id: i64,
    created_at: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    topic: String,
    #[serde(default)]
    original_text: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    important: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    sections: Vec<AnalysisSection>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AnalysisSection {
    title: String,
    items: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct AnalysisOutput {
    topic: String,
    summary: String,
    important: Vec<String>,
    tags: Vec<String>,
    sections: Vec<AnalysisSection>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HealthOutput {
    database: bool,
    unlocked: bool,
    stt_local: bool,
    llm_local: bool,
}

fn app_dir(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    app.path()
        .app_data_dir()
        .map_err(|e| AppError::Message(format!("Не удалось получить каталог приложения: {e}")))
}

fn db_path(app: &tauri::AppHandle) -> AppResult<PathBuf> {
    let dir = app_dir(app)?;
    fs::create_dir_all(&dir)
        .map_err(|e| AppError::Message(format!("Не удалось создать каталог данных: {e}")))?;
    Ok(dir.join(DB_FILE))
}

fn open_db(app: &tauri::AppHandle) -> AppResult<Connection> {
    let path = db_path(app)?;
    let conn = Connection::open(path)?;
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = FULL;
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            salt BLOB NOT NULL,
            version INTEGER NOT NULL,
            verifier BLOB
        );
        CREATE TABLE IF NOT EXISTS sessions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            payload BLOB NOT NULL
        );
        "#,
    )?;

    // Migrate v0.3.x databases that were created before the key verifier existed.
    let verifier_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('meta') WHERE name = 'verifier'",
        [],
        |row| row.get(0),
    )?;
    if verifier_column == 0 {
        conn.execute("ALTER TABLE meta ADD COLUMN verifier BLOB", [])?;
    }

    lock_db_permissions(app)?;
    Ok(conn)
}

fn lock_db_permissions(app: &tauri::AppHandle) -> AppResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let db = db_path(app)?;
        if db.exists() {
            fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).ok();
            let wal = Path::new(&format!("{}-wal", db.display())).to_path_buf();
            if wal.exists() {
                fs::set_permissions(wal, fs::Permissions::from_mode(0o600)).ok();
            }
            let shm = Path::new(&format!("{}-shm", db.display())).to_path_buf();
            if shm.exists() {
                fs::set_permissions(shm, fs::Permissions::from_mode(0o600)).ok();
            }
        }
    }
    Ok(())
}

fn derive_key(password: &str, salt: &[u8]) -> AppResult<[u8; KEY_LEN]> {
    let params =
        argon2::Params::new(19 * 1024, 2, 1, Some(KEY_LEN)).map_err(|_| AppError::Crypto)?;
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|_| AppError::Crypto)?;
    Ok(key)
}

fn make_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    salt
}

fn encrypt(key: &[u8; KEY_LEN], plain: &[u8]) -> AppResult<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| AppError::Crypto)?;
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plain)
        .map_err(|_| AppError::Crypto)?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt(key: &[u8; KEY_LEN], blob: &[u8]) -> AppResult<Vec<u8>> {
    if blob.len() < NONCE_LEN + 16 {
        return Err(AppError::Crypto);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| AppError::Crypto)?;
    cipher
        .decrypt(Nonce::from_slice(&blob[..NONCE_LEN]), &blob[NONCE_LEN..])
        .map_err(|_| AppError::InvalidPassword)
}

fn get_key(state: &State<AppState>) -> AppResult<Zeroizing<[u8; KEY_LEN]>> {
    state
        .key
        .lock()
        .map_err(|_| AppError::Message("Внутренняя блокировка".into()))?
        .as_ref()
        .map(|key| Zeroizing::new(**key))
        .ok_or(AppError::Locked)
}

fn ensure_password(password: &str) -> AppResult<()> {
    let bytes = password.as_bytes();
    if bytes.len() < 10 {
        return Err(AppError::Message(
            "Пароль должен содержать минимум 10 байт.".into(),
        ));
    }
    Ok(())
}

fn is_local_endpoint(raw: &str) -> AppResult<()> {
    if raw.len() > MAX_SETTING_URL_LEN {
        return Err(AppError::UnsafeEndpoint(raw.to_string()));
    }

    let parsed = Url::parse(raw).map_err(|_| AppError::UnsafeEndpoint(raw.to_string()))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(AppError::UnsafeEndpoint(raw.to_string()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AppError::UnsafeEndpoint(raw.to_string()));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| AppError::UnsafeEndpoint(raw.to_string()))?;

    if !matches!(host, "localhost" | "127.0.0.1" | "::1" | "10.0.2.2") {
        return Err(AppError::UnsafeEndpoint(raw.to_string()));
    }
    Ok(())
}

fn validate_neural_settings(settings: &NeuralSettings) -> AppResult<()> {
    if settings.stt_model.trim().is_empty() || settings.stt_model.len() > MAX_SETTING_MODEL_LEN {
        return Err(AppError::Message("Некорректное имя STT-модели.".into()));
    }
    if settings.llm_model.trim().is_empty() || settings.llm_model.len() > MAX_SETTING_MODEL_LEN {
        return Err(AppError::Message("Некорректное имя LLM-модели.".into()));
    }
    is_local_endpoint(&settings.stt_url)?;
    is_local_endpoint(&settings.llm_url)?;
    Ok(())
}

fn wav_header(data_len: u32, sample_rate: u32) -> Vec<u8> {
    let channels = 1u16;
    let bits = 16u16;
    let byte_rate = sample_rate * channels as u32 * bits as u32 / 8;
    let block_align = channels * bits / 8;

    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data_len).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&sample_rate.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&block_align.to_le_bytes());
    h.extend_from_slice(&bits.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data_len.to_le_bytes());
    h
}

fn local_client() -> AppResult<Client> {
    Client::builder()
        // Loopback-only networking is a hard privacy boundary; never use proxy env vars.
        .no_proxy()
        // A malicious/local server must not redirect transcript contents to the internet.
        .redirect(Policy::none())
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(AppError::Network)
}

async fn transcribe_wav(settings: &NeuralSettings, mut wav: Vec<u8>) -> AppResult<String> {
    validate_neural_settings(settings)?;
    let client = local_client()?;
    let part = multipart::Part::bytes(wav.clone())
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| AppError::Message(format!("Не удалось сформировать audio part: {e}")))?;
    let form = multipart::Form::new()
        .text("model", settings.stt_model.clone())
        .part("file", part);

    let response = client
        .post(&settings.stt_url)
        .multipart(form)
        .send()
        .await?
        .error_for_status()?;

    let value: serde_json::Value = response.json().await?;
    let text = value
        .get("text")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("transcript").and_then(|v| v.as_str()))
        .or_else(|| value.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    wav.zeroize();
    Ok(text)
}

fn clean_llm_json(raw: &str) -> &str {
    let trimmed = raw.trim();
    let without_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let without_fence = without_fence
        .strip_suffix("```")
        .unwrap_or(without_fence)
        .trim();

    if without_fence.starts_with('{') && without_fence.ends_with('}') {
        return without_fence;
    }

    match (without_fence.find('{'), without_fence.rfind('}')) {
        (Some(start), Some(end)) if start < end => &without_fence[start..=end],
        _ => without_fence,
    }
}

async fn analyze_text(
    settings: &NeuralSettings,
    subject: &str,
    transcript: &str,
) -> AppResult<AnalysisOutput> {
    validate_neural_settings(settings)?;
    if transcript.len() > MAX_TRANSCRIPT_LEN {
        return Err(AppError::Message(
            "Расшифровка слишком большая для одного анализа.".into(),
        ));
    }
    if subject.len() > MAX_SUBJECT_LEN {
        return Err(AppError::Message("Контекст темы слишком длинный.".into()));
    }

    let system_prompt = r#"
Ты локальный универсальный ассистент для конспектирования и структурирования разговоров, лекций,
совещаний, консультаций, исследований и рабочих обсуждений.

Твоя задача — НЕ угадывать заранее, о какой области идёт речь. Сначала определи тему и под-тему
по исходной расшифровке, затем сожми содержание, сохранив факты и полезные детали.

Поддерживай любые области: физика, математика, программирование, инженерия, строительство,
архитектура, проектирование, медицина, право, история, химия, биология, экономика, бизнес,
финансы, управление проектами, образование, техника, наука и любые другие.

ОБЯЗАТЕЛЬНЫЕ ПРАВИЛА:
1. Оригинальная расшифровка — единственный источник фактов. Ничего не придумывай.
2. Не превращай предположение говорящего в факт. Сохраняй неопределённость: "предполагается",
   "обсуждалось", "не подтверждено" — когда это есть в оригинале.
3. Диагнозы, юридические выводы, расчёты, проектные решения и другие специальные утверждения
   нельзя добавлять от себя.
4. Сохраняй числа, единицы измерения, даты, названия, формулы и технические параметры, если они
    есть в исходнике.
5. Делай краткий, плотный конспект. Не переписывай весь текст в summary.
6. В sections создавай ТОЛЬКО действительно полезные для данной темы разделы. Не нужно искусственно
   заполнять "формулы" для истории или "симптомы" для математики.
7. Для медицины, права, строительства, инженерии и других рискованных областей явно сохраняй
   ограничения/риски, если они были сказаны, и не выдавай конспект за экспертное заключение.
8. Для встреч и бизнеса полезно выделять решения, задачи, ответственных и сроки, но только если
   они реально были сказаны.
9. Для обучения полезно выделять определения, формулы, принципы, примеры и вопросы, если они есть.
10. Для инженерии/строительства/проектирования полезно выделять требования, размеры, материалы,
    допуски, зависимости, риски, этапы и решения — только при наличии в исходнике.

Верни СТРОГО валидный JSON без markdown и без ```:
{
  "topic": "определённая область и подтема в 3-10 словах",
  "summary": "сжатое связное содержание в нескольких предложениях или абзацах",
  "important": ["самые важные факты или выводы, обычно 3-12 пунктов"],
  "tags": ["релевантные темы и термины"],
  "sections": [
    {"title": "релевантный раздел", "items": ["факт", "факт"]}
  ]
}

Если какого-то вида информации нет, не создавай пустой или выдуманный раздел.
"#;

    let user_prompt = format!(
        "Контекст пользователя (может быть пустым): {subject}\n\nОригинальная расшифровка:\n{transcript}\n\nПроанализируй именно этот текст."
    );

    // Выполняем инференс в отдельном блокирующем потоке (tokio::task::spawn_blocking),
    // чтобы не вешать асинхронный рантайм Tauri/Tokio тяжелыми вычислениями CPU.
    let model_path = settings.llm_model.clone();

    // Запускаем инференс в отдельном блокирующем потоке
    let join_result = tokio::task::spawn_blocking(move || {
        // 1. Загрузка модели и токенизатора
        let model = load_llama_model(&model_path)
            .map_err(|e| format!("Не удалось загрузить модель GGUF: {e}"))?;
        
        let gguf = GgufFile::open(&model_path)
            .map_err(|e| format!("Не удалось открыть GGUF файл: {e}"))?;
            
        let tokenizer = Tokenizer::from_gguf(&gguf)
            .map_err(|e| format!("Не удалось инициализировать токенизатор: {e}"))?;

        // 2. Инициализация бэкенда и контекста
        let backend = Arc::new(llama_rs::backend::cpu::CpuBackend::new());
        let mut ctx = InferenceContext::new(model.config(), backend);

        // 3. Настройка семплера
        let sampler_config = SamplerConfig {
            temperature: 0.1,
            top_k: 40,
            top_p: 0.95,
            ..Default::default()
        };
        let mut sampler = Sampler::new(sampler_config, model.config().vocab_size);

        // 4. Формирование чат-промпта
        let full_prompt = format!(
            "<|im_start|>system\n{system_prompt}<|im_end|>\n<|im_start|>user\n{user_prompt}<|im_end|>\n<|im_start|>assistant\n"
        );

        let tokens = tokenizer.encode(&full_prompt, true)
            .map_err(|e| format!("Ошибка токенизации промпта: {e}"))?;

        let mut output_tokens = tokens.clone();
        let mut generated_text = String::new();
        
        // Предзаполнение контекста (prefill)
        model.forward(&output_tokens, &mut ctx)
            .map_err(|e| format!("Ошибка prefill этапа: {e}"))?;

        // Генерация токенов
        for _ in 0..2048 {
            let last_token_slice = &output_tokens[output_tokens.len() - 1..];
            let logits = model.forward(last_token_slice, &mut ctx)
                .map_err(|e| format!("Ошибка forward прохода: {e}"))?;
                
            let next_token = sampler.sample(&logits, &output_tokens);
            output_tokens.push(next_token);

            if let Ok(text) = tokenizer.decode(&[next_token]) {
                generated_text.push_str(&text);
                if text.contains("<|im_end|>") {
                    break;
                }
            }
        }

        Ok::<String, String>(generated_text)
    })
    .await;

    // Аккуратно обрабатываем результат потока и переводим ошибки в AppError::Message
    let raw_output = match join_result {
        Ok(Ok(text)) => text,
        Ok(Err(e)) => return Err(AppError::Message(format!("Ошибка внутри LLM потока: {e}"))),
        Err(e) => return Err(AppError::Message(format!("Ошибка выполнения потока (JoinError): {e}"))),
    };

    let trimmed = raw_output.replace("<|im_end|>", "");
    let cleaned_raw = clean_llm_json(trimmed.trim());

    let parsed: AnalysisOutput = serde_json::from_str(cleaned_raw)
        .map_err(|e| AppError::Message(format!("Локальная LLM вернула невалидный JSON: {e}. Сырой вывод: {raw_output}")))?;

    Ok(parsed)
}

#[tauri::command]
fn database_status(app: tauri::AppHandle, state: State<AppState>) -> Result<HealthOutput, String> {
    let conn = open_db(&app).map_err(|e| e.to_string())?;
    let initialized = conn
        .query_row("SELECT COUNT(*) FROM meta WHERE id=1", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|v| v > 0)
        .unwrap_or(false);
    let unlocked = state.key.lock().map(|g| g.is_some()).unwrap_or(false);
    Ok(HealthOutput {
        database: initialized,
        unlocked,
        stt_local: false,
        llm_local: false,
    })
}

#[tauri::command]
fn initialize_database(
    app: tauri::AppHandle,
    state: State<AppState>,
    password: String,
) -> Result<(), String> {
    let password = Zeroizing::new(password);
    let conn = open_db(&app).map_err(|e| e.to_string())?;

    let exists: i64 = conn
        .query_row("SELECT COUNT(*) FROM meta WHERE id=1", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;

    if exists > 0 {
        return Err("Хранилище уже создано. Используйте разблокировку.".into());
    }

    let salt = make_salt();
    ensure_password(password.as_str()).map_err(|e| e.to_string())?;
    let key = derive_key(password.as_str(), &salt).map_err(|e| e.to_string())?;
    let verifier = encrypt(&key, KEY_CHECK).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO meta(id,salt,version,verifier) VALUES (1,?1,2,?2)",
        params![salt.to_vec(), verifier],
    )
    .map_err(|e| e.to_string())?;

    *state
        .key
        .lock()
        .map_err(|_| "Внутренняя блокировка".to_string())? = Some(Zeroizing::new(key));
    Ok(())
}

#[tauri::command]
fn unlock_database(
    app: tauri::AppHandle,
    state: State<AppState>,
    password: String,
) -> Result<(), String> {
    let password = Zeroizing::new(password);
    let conn = open_db(&app).map_err(|e| e.to_string())?;
    let salt: Vec<u8> = conn
        .query_row("SELECT salt FROM meta WHERE id=1", [], |row| row.get(0))
        .map_err(|_| AppError::NotInitialized.to_string())?;

    ensure_password(password.as_str()).map_err(|e| e.to_string())?;
    let key = derive_key(password.as_str(), &salt).map_err(|e| e.to_string())?;

    let verifier: Option<Vec<u8>> = conn
        .query_row("SELECT verifier FROM meta WHERE id=1", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;

    match verifier {
        Some(blob) => {
            let plain = decrypt(&key, &blob).map_err(|_| AppError::InvalidPassword.to_string())?;
            if plain.as_slice() != KEY_CHECK {
                return Err(AppError::InvalidPassword.to_string());
            }
        }
        None => {
            // Legacy database: if it contains a session, validate the derived key
            // against that encrypted payload and then upgrade the metadata.
            if let Ok(blob) = conn.query_row(
                "SELECT payload FROM sessions ORDER BY id LIMIT 1",
                [],
                |r| r.get::<_, Vec<u8>>(0),
            ) {
                decrypt(&key, &blob).map_err(|_| AppError::InvalidPassword.to_string())?;
                let verifier = encrypt(&key, KEY_CHECK).map_err(|e| e.to_string())?;
                conn.execute(
                    "UPDATE meta SET verifier=?1, version=2 WHERE id=1",
                    params![verifier],
                )
                .map_err(|e| e.to_string())?;
            } else {
                return Err(
                    "Старое пустое хранилище нельзя проверить безопасно. Создайте его заново."
                        .into(),
                );
            }
        }
    }

    *state
        .key
        .lock()
        .map_err(|_| "Внутренняя блокировка".to_string())? = Some(Zeroizing::new(key));
    Ok(())
}

#[tauri::command]
fn lock_database(state: State<AppState>) -> Result<(), String> {
    state
        .key
        .lock()
        .map_err(|_| "Внутренняя блокировка".to_string())?
        .take();

    let mut audio = state
        .audio
        .lock()
        .map_err(|_| "Аудиобуфер заблокирован".to_string())?;
    audio.pcm.zeroize();
    audio.pcm.clear();
    audio.sample_rate = 0;
    Ok(())
}

#[tauri::command]
fn push_audio_chunk(
    state: State<AppState>,
    audio_data: Vec<u8>,
    sample_rate: u32,
) -> Result<(), String> {
    if audio_data.is_empty() {
        return Ok(());
    }
    if audio_data.len() > 2 * 1024 * 1024 {
        return Err("Слишком большой аудиобуфер за один IPC-вызов.".into());
    }

    let mut audio = state
        .audio
        .lock()
        .map_err(|_| "Не удалось заблокировать аудиобуфер".to_string())?;

    if audio.pcm.len() + audio_data.len() > MAX_AUDIO_BYTES {
        return Err("Превышен лимит временного аудиобуфера. Остановите запись.".into());
    }

    if !(8000..=48000).contains(&sample_rate) {
        return Err("Недопустимая частота дискретизации: ожидается 8000..48000 Гц.".into());
    }

    if audio.sample_rate != 0 && audio.sample_rate != sample_rate {
        return Err("Частота дискретизации изменилась во время одной записи.".into());
    }

    audio.sample_rate = sample_rate;
    audio.pcm.extend_from_slice(&audio_data);
    Ok(())
}

#[tauri::command]
async fn transcribe_pending_audio(
    state: State<'_, AppState>,
    settings: NeuralSettings,
) -> Result<String, String> {
    let (pcm, sample_rate) = {
        let mut audio = state
            .audio
            .lock()
            .map_err(|_| "Аудиобуфер заблокирован".to_string())?;
        if audio.pcm.is_empty() {
            return Ok(String::new());
        }
        let pcm = std::mem::take(&mut audio.pcm);
        (pcm, audio.sample_rate.max(8000))
    };

    let data_len =
        u32::try_from(pcm.len()).map_err(|_| "Аудиобуфер слишком велик для WAV.".to_string())?;
    let mut wav = wav_header(data_len, sample_rate);
    wav.extend_from_slice(&pcm);

    match transcribe_wav(&settings, wav).await {
        Ok(text) => {
            // Best-effort wipe of the temporary PCM owned by this call.
            // The WAV Vec itself is dropped after this branch.
            Ok(text)
        }
        Err(err) => {
            let mut audio = state
                .audio
                .lock()
                .map_err(|_| "Аудиобуфер заблокирован".to_string())?;
            let mut restored = pcm;
            restored.append(&mut audio.pcm);
            audio.pcm = restored;
            Err(err.to_string())
        }
    }
}

#[tauri::command]
async fn analyze_transcript(
    settings: NeuralSettings,
    subject: String,
    transcript: String,
) -> Result<AnalysisOutput, String> {
    if transcript.trim().is_empty() {
        return Err("Нет текста для анализа.".into());
    }
    validate_neural_settings(&settings).map_err(|e| e.to_string())?;
    analyze_text(&settings, &subject, &transcript)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn save_session(
    app: tauri::AppHandle,
    state: State<AppState>,
    mode: String,
    subject: String,
    topic: String,
    original_text: String,
    summary: String,
    important: Vec<String>,
    tags: Vec<String>,
    sections: Vec<AnalysisSection>,
) -> Result<i64, String> {
    let key = get_key(&state).map_err(|e| e.to_string())?;
    if subject.len() > MAX_SUBJECT_LEN {
        return Err("Тема слишком длинная.".into());
    }
    if original_text.len() > MAX_TRANSCRIPT_LEN {
        return Err("Оригинальная расшифровка слишком большая для сохранения.".into());
    }
    let conn = open_db(&app).map_err(|e| e.to_string())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "Некорректные системные часы".to_string())?
        .as_secs()
        .to_string();

    let payload = SessionPayload {
        id: 0,
        created_at: now,
        mode,
        subject,
        topic,
        original_text,
        summary,
        important,
        tags,
        sections,
    };

    let mut raw = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let encrypted = encrypt(&key, &raw).map_err(|e| e.to_string())?;
    raw.zeroize();

    conn.execute(
        "INSERT INTO sessions(payload) VALUES (?1)",
        params![encrypted],
    )
    .map_err(|e| e.to_string())?;

    let id = conn.last_insert_rowid();

    // Обновляем id внутри зашифрованного payload, чтобы запись была самодостаточной.
    let mut saved = payload;
    saved.id = id;
    let mut saved_raw = serde_json::to_vec(&saved).map_err(|e| e.to_string())?;
    let encrypted = encrypt(&key, &saved_raw).map_err(|e| e.to_string())?;
    saved_raw.zeroize();
    conn.execute(
        "UPDATE sessions SET payload=?1 WHERE id=?2",
        params![encrypted, id],
    )
    .map_err(|e| e.to_string())?;

    Ok(id)
}

#[tauri::command]
fn list_sessions(
    app: tauri::AppHandle,
    state: State<AppState>,
) -> Result<Vec<SessionPayload>, String> {
    let key = get_key(&state).map_err(|e| e.to_string())?;
    let conn = open_db(&app).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("SELECT id,payload FROM sessions ORDER BY id DESC")
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| e.to_string())?;

    let mut output = Vec::new();
    for row in rows {
        let (id, blob) = row.map_err(|e| e.to_string())?;
        let mut decrypted = decrypt(&key, &blob).map_err(|e| e.to_string())?;
        let mut payload: SessionPayload =
            serde_json::from_slice(&decrypted).map_err(|e| e.to_string())?;
        decrypted.zeroize();
        payload.id = id;
        output.push(payload);
    }
    Ok(output)
}

#[tauri::command]
async fn check_local_ai(settings: NeuralSettings) -> Result<String, String> {
    validate_neural_settings(&settings).map_err(|e| e.to_string())?;

    // Отправляем в STT только 1 секунду тишины: это проверяет реальный локальный
    // канал, но не содержит пользовательских данных.
    let mut silence_wav = wav_header(32000, 16000);
    silence_wav.resize(44 + 32000, 0);
    transcribe_wav(&settings, silence_wav)
        .await
        .map_err(|e| format!("STT недоступна: {e}"))?;

    let body = serde_json::json!({
        "model": settings.llm_model,
        "stream": false,
        "options": {"temperature": 0},
        "messages": [
            {"role":"system","content":"Ответь только JSON: {\"ok\":true}"},
            {"role":"user","content":"Проверка локального канала. Ответь только JSON."}
        ]
    });

    let client = local_client().map_err(|e| e.to_string())?;
    let response = client
        .post(&settings.llm_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("LLM недоступна: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("LLM вернула HTTP {}", response.status()));
    }

    Ok("STT и LLM отвечают через loopback. Внешний endpoint не используется.".into())
}

#[tauri::command]
fn clear_audio_buffer(state: State<AppState>) -> Result<(), String> {
    let mut audio = state
        .audio
        .lock()
        .map_err(|_| "Аудиобуфер заблокирован".to_string())?;
    audio.pcm.zeroize();
    audio.pcm.clear();
    Ok(())
}

#[tauri::command]
fn get_default_settings() -> NeuralSettings {
    NeuralSettings::default()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            database_status,
            initialize_database,
            unlock_database,
            lock_database,
            push_audio_chunk,
            transcribe_pending_audio,
            analyze_transcript,
            save_session,
            list_sessions,
            clear_audio_buffer,
            get_default_settings,
            check_local_ai
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [7u8; KEY_LEN];
        let input = "секретный текст".as_bytes();
        let encrypted = encrypt(&key, input).expect("encrypt");
        assert_ne!(encrypted, input);
        let decrypted = decrypt(&key, &encrypted).expect("decrypt");
        assert_eq!(decrypted, input);
    }

    #[test]
    fn rejects_non_loopback_endpoint() {
        assert!(is_local_endpoint("https://example.com/v1").is_err());
        assert!(is_local_endpoint("http://127.0.0.1:8000/v1").is_ok());
        assert!(is_local_endpoint("http://localhost:11434/api/chat").is_ok());
        assert!(is_local_endpoint("http://10.0.2.2:8000/v1").is_ok());
    }

    #[test]
    fn wav_header_is_valid_riff() {
        let h = wav_header(32000, 16000);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 32000);
    }
}
