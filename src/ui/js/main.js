const { invoke } = window.__TAURI__.core;

const currentMode = "auto";
let settings = null;
let mediaStream = null;
let audioContext = null;
let sourceNode = null;
let processorNode = null;
let monitorGainNode = null;
let recording = false;
let pendingSamples = [];
let transcript = "";
let lastAnalysis = null;
let timerId = null;
let recordingStartedAt = 0;
let flushTimer = null;
let vaultReady = false;
let transcriptBusy = false;
let audioFlushPromise = null;
let pendingSampleCount = 0;
let workletDrainResolver = null;
let recordingSampleRate = 48000;

const $ = (id) => document.getElementById(id);

const recordBtn = $("recordBtn");
const recordStatus = $("recordStatus");
const transcriptEl = $("transcript");
const summaryEl = $("summary");
const importantGrid = $("importantGrid");
const saveBtn = $("saveBtn");
const errorBox = $("errorBox");
const timerEl = $("timer");

function setError(message) {
  errorBox.textContent = message || "";
  errorBox.classList.toggle("hidden", !message);
}

function setVaultError(message) {
  const el = $("vaultError");
  el.textContent = message || "";
  el.classList.toggle("hidden", !message);
}

function setStatus(text, kind = "idle") {
  recordStatus.textContent = text;
  recordStatus.className = `status-pill ${kind}`;
}

function escapeHtml(s) {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

function formatTime(sec) {
  const s = Math.max(0, Math.floor(sec));
  return `${String(Math.floor(s / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
}

function setAnalysisMode() {
  $("pageTitle").textContent = "Универсальный голосовой конспект";
  $("pageSubtitle").textContent =
    "Нейросеть сама определяет тему и выделяет главное — от физики и медицины до строительства, бизнеса и истории.";
  $("subjectInput").placeholder =
    "Необязательно: физика / проект здания / приём пациента / совещание по бизнесу";
  $("importantTitle").textContent = "Что важно по теме";
}

async function loadSettings() {
  settings = await invoke("get_default_settings");
}

function updateSecurity(unlocked) {
  vaultReady = unlocked;
  $("securityState").className = `security-pill ${unlocked ? "unlocked" : "locked"}`;
  $("securityText").textContent = unlocked ? "Хранилище разблокировано" : "Хранилище заблокировано";
  $("lockBtn").classList.toggle("hidden", !unlocked);
  if (unlocked) loadSessions().catch(showTopError);
}

async function initVault() {
  const status = await invoke("database_status");
  if (!status.database) {
    $("vaultTitle").textContent = "Создайте пароль";
    $("vaultDescription").textContent =
      "Это пароль от локального хранилища. Он не отправляется в сеть и не сохраняется.";
    $("vaultPassword2Wrap").classList.remove("hidden");
    $("vaultPassword2").required = true;
    $("vaultDialog").showModal();
  } else if (!status.unlocked) {
    $("vaultTitle").textContent = "Разблокируйте хранилище";
    $("vaultDescription").textContent = "Введите пароль, заданный при первом запуске.";
    $("vaultPassword2Wrap").classList.add("hidden");
    $("vaultPassword2").required = false;
    $("vaultDialog").showModal();
  } else {
    updateSecurity(true);
  }
}

$("vaultForm").addEventListener("submit", async (e) => {
  e.preventDefault();
  setVaultError("");

  const password = $("vaultPassword").value;
  const password2 = $("vaultPassword2").value;
  const needsConfirm = !$("vaultPassword2Wrap").classList.contains("hidden");

  if (needsConfirm && password !== password2) {
    setVaultError("Пароли не совпадают.");
    return;
  }

  try {
    const status = await invoke("database_status");
    if (!status.database) {
      await invoke("initialize_database", { password });
    } else {
      await invoke("unlock_database", { password });
    }

    $("vaultPassword").value = "";
    $("vaultPassword2").value = "";
    $("vaultDialog").close();
    updateSecurity(true);
  } catch (err) {
    setVaultError(String(err));
  }
});

$("lockBtn").addEventListener("click", async () => {
  if (recording) await stopRecording();
  await invoke("lock_database");
  updateSecurity(false);
  $("sessionList").innerHTML = '<div class="empty-state">Хранилище заблокировано.</div>';
  transcript = "";
  renderTranscript();
});

function resampleFloat32(samples, inputRate, outputRate) {
  if (inputRate === outputRate) return samples;
  const ratio = inputRate / outputRate;
  const newLength = Math.floor(samples.length / ratio);
  const out = new Float32Array(newLength);

  for (let i = 0; i < newLength; i++) {
    const pos = i * ratio;
    const left = Math.floor(pos);
    const right = Math.min(left + 1, samples.length - 1);
    const frac = pos - left;
    out[i] = samples[left] * (1 - frac) + samples[right] * frac;
  }
  return out;
}

function floatToPcm16(samples) {
  const out = new Uint8Array(samples.length * 2);
  const view = new DataView(out.buffer);

  for (let i = 0; i < samples.length; i++) {
    const value = Math.max(-1, Math.min(1, samples[i]));
    view.setInt16(i * 2, value < 0 ? value * 0x8000 : value * 0x7fff, true);
  }
  return out;
}

function flushAudio(force = false) {
  if (audioFlushPromise) return audioFlushPromise;
  if (!pendingSamples.length) return Promise.resolve();

  const blocks = pendingSamples;
  pendingSamples = [];
  pendingSampleCount = 0;

  audioFlushPromise = (async () => {
    const joined = new Float32Array(blocks.reduce((total, block) => total + block.length, 0));
    let offset = 0;
    for (const block of blocks) {
      joined.set(block, offset);
      offset += block.length;
    }

    const inputRate = recordingSampleRate || 48000;
    const resampled = resampleFloat32(joined, inputRate, 16000);
    const pcm = floatToPcm16(resampled);

    try {
      await invoke("push_audio_chunk", {
        audioData: Array.from(pcm),
        sampleRate: 16000,
      });

      if (force) await flushTranscript();
    } catch (err) {
      pendingSamples = blocks.concat(pendingSamples);
      pendingSampleCount = pendingSamples.reduce((total, block) => total + block.length, 0);
      showTopError(`Не удалось передать аудио: ${err}`);
    } finally {
      audioFlushPromise = null;
    }
  })();

  return audioFlushPromise;
}

async function flushTranscript() {
  if (transcriptBusy) return;
  transcriptBusy = true;
  try {
    $("sttState").textContent = "STT: обработка…";
    const text = await invoke("transcribe_pending_audio", { settings });
    if (text && text.trim()) {
      transcript = `${transcript} ${text.trim()}`.trim();
      renderTranscript();
      $("sttState").textContent = "STT: локальный ✓";
    } else {
      $("sttState").textContent = "STT: локальный";
    }
  } catch (err) {
    $("sttState").textContent = "STT: ошибка";
    showTopError(`STT не ответил: ${err}`);
  } finally {
    transcriptBusy = false;
  }
}

function renderTranscript() {
  if (!transcript.trim()) {
    transcriptEl.textContent = "После записи здесь будет полный текст разговора.";
    return;
  }
  transcriptEl.textContent = transcript;
}

function stopMedia() {
  if (processorNode) {
    try { processorNode.disconnect(); } catch { }
  }
  if (sourceNode) {
    try { sourceNode.disconnect(); } catch { }
  }
  if (monitorGainNode) {
    try { monitorGainNode.disconnect(); } catch { }
  }
  if (audioContext) {
    try { audioContext.close(); } catch { }
  }
  if (mediaStream) mediaStream.getTracks().forEach((t) => t.stop());

  processorNode = null;
  monitorGainNode = null;
  sourceNode = null;
  audioContext = null;
  mediaStream = null;
}

async function stopRecording() {
  if (!recording) return;

  recording = false;
  clearInterval(timerId);
  clearInterval(flushTimer);

  setStatus("Обработка…", "processing");
  $("micHint").textContent = "Передаём последние данные в локальный STT…";
  recordBtn.classList.remove("recording");
  $("waveform").classList.remove("active");

  if (processorNode) {
    await new Promise((resolve) => {
      workletDrainResolver = resolve;
      try {
        processorNode.port.postMessage({ type: "drain" });
      } catch {
        workletDrainResolver = null;
        resolve();
      }
      setTimeout(() => {
        if (workletDrainResolver) {
          workletDrainResolver = null;
          resolve();
        }
      }, 1000);
    });
  }

  stopMedia();
  await flushAudio(false);
  await flushTranscript();

  setStatus("Готово", "idle");
  $("micHint").textContent = "Запись завершена · можно сохранить";
  await analyze();
}

async function startRecording() {
  setError("");
  if (!vaultReady) {
    showTopError("Сначала разблокируйте локальное хранилище.");
    return;
  }

  try {
    if (!navigator.mediaDevices?.getUserMedia) {
      throw new Error(
        `getUserMedia недоступен. Secure context=${window.isSecureContext}.`
      );
    }
    await invoke("clear_audio_buffer");
    mediaStream = await navigator.mediaDevices.getUserMedia({
      audio: {
        channelCount: 1,
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
      },
      video: false,
    });

    audioContext = new AudioContext();
    recordingSampleRate = audioContext.sampleRate || 48000;
    console.log("Loading worklet...");
    await audioContext.audioWorklet.addModule("/ui/js/recorder-worklet.js");
    console.log("Worklet loaded");
    sourceNode = audioContext.createMediaStreamSource(mediaStream);
    processorNode = new AudioWorkletNode(audioContext, "pcm-recorder");

    monitorGainNode = audioContext.createGain();
    monitorGainNode.gain.value = 0;

    processorNode.port.onmessage = (event) => {
      if (event.data && event.data.type === "drained") {
        workletDrainResolver?.();
        workletDrainResolver = null;
        return;
      }
      const samples = event.data instanceof Float32Array
        ? event.data
        : new Float32Array(event.data);
      pendingSamples.push(samples);
      pendingSampleCount += samples.length;
      if (pendingSampleCount >= audioContext.sampleRate * 0.9) {
        void flushAudio(false);
      }
    };

    sourceNode.connect(processorNode);
    processorNode.connect(monitorGainNode);
    monitorGainNode.connect(audioContext.destination);

    recording = true;
    recordingStartedAt = Date.now();
    timerId = setInterval(() => {
      timerEl.textContent = formatTime((Date.now() - recordingStartedAt) / 1000);
    }, 250);

    flushTimer = setInterval(async () => {
      await flushAudio(false);
      await flushTranscript();
    }, 7000);

    setStatus("Запись…", "recording");
    recordBtn.classList.add("recording");
    $("micHint").textContent = "Идёт запись · нажмите, чтобы остановить";
    $("waveform").classList.add("active");
  } catch (err) {
    console.error("START_RECORDING_ERROR", err);
    stopMedia();
    showTopError(
      `Ошибка записи: ${err?.name || "Unknown"}: ${err?.message || err}`
    );
  }
}

recordBtn.addEventListener("click", () => {
  if (recording) stopRecording();
  else startRecording();
});

async function analyze() {
  if (!transcript.trim()) {
    summaryEl.textContent = "STT не вернул текст. Проверьте локальный STT endpoint.";
    return;
  }

  $("analysisState").textContent = "анализ…";
  try {
    lastAnalysis = await invoke("analyze_transcript", {
      settings,
      subject: $("subjectInput").value.trim() || "",
      transcript,
    });

    renderAnalysis(lastAnalysis);
    saveBtn.disabled = false;
    $("analysisState").textContent = "готово";
  } catch (err) {
    $("analysisState").textContent = "ошибка";
    showTopError(`Анализ не выполнен: ${err}`);
  }
}

function renderAnalysis(analysis) {
  const topic = String(analysis?.topic || "Тема не определена").trim();
  const sections = Array.isArray(analysis?.sections) ? analysis.sections : [];
  const important = Array.isArray(analysis?.important) ? analysis.important : [];
  const tags = Array.isArray(analysis?.tags) ? analysis.tags : [];

  summaryEl.innerHTML = `
    <div class="topic-badge">${escapeHtml(topic)}</div>
    <div class="analysis-summary">${escapeHtml(analysis?.summary || "Локальная модель не вернула краткое содержание.")}</div>
  `;

  const cards = [];
  if (important.length) {
    cards.push(`
      <section class="analysis-section">
        <div class="analysis-section-head">
          <span class="section-index">01</span>
          <h3>Самое важное</h3>
        </div>
        <div class="analysis-items">
          ${important.map(item => `<div class="analysis-item">${escapeHtml(item)}</div>`).join("")}
        </div>
      </section>
    `);
  }

  sections.forEach((section, index) => {
    const title = String(section?.title || "").trim();
    const items = Array.isArray(section?.items) ? section.items.filter(Boolean) : [];
    if (!title || !items.length) return;

    cards.push(`
      <section class="analysis-section">
        <div class="analysis-section-head">
          <span class="section-index">${String(index + 2).padStart(2, "0")}</span>
          <h3>${escapeHtml(title)}</h3>
        </div>
        <div class="analysis-items">
          ${items.map(item => `<div class="analysis-item">${escapeHtml(item)}</div>`).join("")}
        </div>
      </section>
    `);
  });

  if (tags.length) {
    cards.push(`
      <section class="analysis-section tags-section">
        <div class="analysis-section-head">
          <span class="section-index">#</span>
          <h3>Темы и термины</h3>
        </div>
        <div class="tag-list">${tags.map(tag => `<span class="topic-tag">${escapeHtml(tag)}</span>`).join("")}</div>
      </section>
    `);
  }

  importantGrid.innerHTML = cards.length
    ? cards.join("")
    : '<div class="placeholder-card">Нейросеть не нашла структурированных пунктов без домыслов.</div>';

  saveBtn.disabled = false;
}

saveBtn.addEventListener("click", async () => {
  if (!vaultReady || !lastAnalysis || !transcript.trim()) return;

  try {
    const id = await invoke("save_session", {
      mode: currentMode,
      subject: $("subjectInput").value.trim() || "",
      topic: lastAnalysis.topic || "Не определено",
      originalText: transcript,
      summary: lastAnalysis.summary || "",
      important: lastAnalysis.important || [],
      tags: lastAnalysis.tags || [],
      sections: lastAnalysis.sections || [],
    });
    saveBtn.textContent = `Сохранено #${id}`;
    saveBtn.disabled = true;
    await loadSessions();
  } catch (err) {
    showTopError(`Не удалось сохранить: ${err}`);
  }
});

async function loadSessions() {
  if (!vaultReady) return;
  try {
    const sessions = await invoke("list_sessions");
    $("sessionList").innerHTML = "";

    if (!sessions.length) {
      $("sessionList").innerHTML = '<div class="empty-state">Сохранённых сессий пока нет.</div>';
      return;
    }

    for (const session of sessions) {
      const item = document.createElement("button");
      item.className = "session-item";
      item.innerHTML = `
        <span class="session-mode">AUTO</span>
        <span class="session-main">
          <strong>${escapeHtml(session.topic || session.subject || "Без темы")}</strong>
          <small>${escapeHtml(session.subject || session.summary || (session.originalText || "").slice(0, 80))}</small>
        </span>`;
      item.addEventListener("click", () => {
        transcript = session.originalText || "";
        lastAnalysis = {
          topic: session.topic || "Старая запись",
          summary: session.summary || "",
          important: session.important || [],
          tags: session.tags || [],
          sections: session.sections || [],
        };
        $("subjectInput").value = session.subject || "";
        renderTranscript();
        renderAnalysis(lastAnalysis);
        saveBtn.disabled = true;
        $("analysisState").textContent = "из БД";
      });
      $("sessionList").appendChild(item);
    }
  } catch (err) {
    showTopError(`Не удалось загрузить БД: ${err}`);
  }
}

$("refreshSessions").addEventListener("click", () => loadSessions());
$("copyTranscriptBtn").addEventListener("click", async () => {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(transcript || "");
    } else {
      const area = document.createElement("textarea");
      area.value = transcript || "";
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    }
    $("copyTranscriptBtn").textContent = "Скопировано";
    setTimeout(() => $("copyTranscriptBtn").textContent = "Копировать", 1200);
  } catch (err) {
    showTopError(`Не удалось скопировать текст: ${err}`);
  }
});

$("checkAiBtn").addEventListener("click", async () => {
  try {
    const result = await invoke("check_local_ai", { settings });
    setError(`✓ ${result}`);
    setTimeout(() => setError(""), 5000);
  } catch (err) {
    setError(`Проверка AI: ${err}`);
  }
  $("settingsDialog").showModal();
});

$("cancelSettings").addEventListener("click", () => $("settingsDialog").close());

$("settingsForm").addEventListener("submit", (e) => {
  e.preventDefault();
  settings = {
    sttUrl: $("sttUrl").value.trim(),
    sttModel: $("sttModel").value.trim(),
    llmUrl: $("llmUrl").value.trim(),
    llmModel: $("llmModel").value.trim(),
  };
  $("settingsDialog").close();
  $("sttState").textContent = `STT: ${settings.sttModel}`;
});

function showTopError(message) {
  setError(message);
  console.error(message);
}

window.addEventListener("beforeunload", () => {
  if (recording) stopRecording();
});

async function boot() {
  try {
    await loadSettings();
    setAnalysisMode();
    await initVault();

    $("sttUrl").value = settings.sttUrl;
    $("sttModel").value = settings.sttModel;
    $("llmUrl").value = settings.llmUrl;
    $("llmModel").value = settings.llmModel;
  } catch (err) {
    showTopError(`Ошибка запуска: ${err}`);
  }
}

boot();
