# 🎬 Video Clipper API v0.2

REST API + WebSocket сервер: загружаешь видео — получаешь нарезанные клипы с самыми интересными моментами.

## Что нового в v0.2

- **WebSocket live-прогресс** — `ws://host/api/jobs/:id/ws` стримит статус в реальном времени
- **SQLite хранилище** — задачи переживают рестарт сервера
- **Очередь с concurrency limit** — `MAX_CONCURRENT_JOBS=2` не даёт перегрузить сервер
- **Vertical crop 9:16** — `vertical_crop=true` выдаёт клипы 1080×1920 для Shorts/Reels/TikTok

## Архитектура

```
Client ──POST /api/jobs──▶ Axum API ──▶ SQLite ──▶ Queue (mpsc)
  │                                                    │
  │◀─── WS /api/jobs/:id/ws ◀── broadcast ◀───────────┤
  │                                              ┌─────▼──────┐
  │                                              │  Semaphore  │
  │                                              │ (N permits) │
  │                                              └─────┬──────┘
  │                                                    │
  │         ┌──────────────────────────────────────────▼──────┐
  │         │  Pipeline (per job)                             │
  │         │  1. FFmpeg → extract audio (mp3 16kHz)          │
  │         │  2. OpenRouter STT → timestamped transcript     │
  │         │  3. OpenRouter LLM → highlight analysis         │
  │         │  4. FFmpeg → cut clips (+ optional 9:16 crop)   │
  │         └─────────────────────────────────────────────────┘
  │
  ├──GET /api/jobs/:id──▶ Job status + highlights + clips list
  └──GET /api/jobs/:id/clips/:file──▶ Download clip
```

## Сборка

```bash
cargo build --release
```

## Запуск

```bash
export OPENROUTER_API_KEY="sk-or-v1-..."

./target/release/video-clipper \
  --port 3000 \
  --stt-model "google/gemini-2.5-flash" \
  --analysis-model "anthropic/claude-sonnet-4" \
  --max-concurrent-jobs 3
```

## Переменные окружения

| Переменная | Default | Описание |
|---|---|---|
| `OPENROUTER_API_KEY` | — | Ключ OpenRouter (обязателен) |
| `HOST` | `0.0.0.0` | Хост |
| `PORT` | `8080` | Порт |
| `STT_MODEL` | `google/gemini-2.5-flash` | Модель для speech-to-text |
| `ANALYSIS_MODEL` | `anthropic/claude-sonnet-4` | Модель для анализа |
| `MAX_CONCURRENT_JOBS` | `2` | Макс. параллельных обработок |
| `MAX_CLIPS` | `20` | Клипов на видео |
| `MIN_CLIP_DURATION` | `10` | Мин. длина клипа (сек) |
| `MAX_CLIP_DURATION` | `60` | Макс. длина клипа (сек) |
| `CHUNK_DURATION` | `300` | Аудио-чанк для STT (сек) |
| `DATA_DIR` | `./data` | Директория данных |
| `DB_FILE` | `clipper.db` | Файл SQLite БД |
| `MAX_UPLOAD_MB` | `2048` | Лимит загрузки (МБ) |

## API

### `POST /api/jobs` — Создать задачу

Multipart form-data:

| Поле | Тип | Описание |
|---|---|---|
| `file` | file | Видеофайл (обязателен) |
| `language` | string | Язык (`ru`, `en`, ...) |
| `max_clips` | number | Макс. клипов |
| `min_clip_duration` | number | Мин. длина клипа |
| `max_clip_duration` | number | Макс. длина клипа |
| `vertical_crop` | bool | `true` — кропить в 9:16 (1080×1920) |

```bash
curl -X POST http://localhost:8080/api/jobs \
  -F "file=@podcast.mp4" \
  -F "language=ru" \
  -F "vertical_crop=true" \
  -F "max_clips=5"
```

Response `202`:
```json
{
  "id": "550e8400-...",
  "status": "queued",
  "progress": 0,
  "vertical_crop": true,
  ...
}
```

### `GET /api/jobs/:id/ws` — WebSocket прогресс

```javascript
const ws = new WebSocket('ws://localhost:8080/api/jobs/550e8400-.../ws');
ws.onmessage = (e) => {
  const event = JSON.parse(e.data);
  console.log(`[${event.status}] ${event.progress}% — ${event.message}`);
  // { "job_id": "...", "status": "transcribing", "progress": 35, "message": "Transcribed 42 segments" }
};
```

**Events flow:**
```
queued → extracting_audio → transcribing → analyzing → cutting → completed
                                                                   └─ or → failed
```

Соединение закрывается автоматически при `completed` или `failed`.

### `GET /api/jobs` — Список задач

```bash
curl http://localhost:8080/api/jobs?status=completed
```

### `GET /api/jobs/:id` — Детали + клипы

```bash
curl http://localhost:8080/api/jobs/550e8400-...
```

### `GET /api/jobs/:id/clips/:filename` — Скачать клип

```bash
curl -O http://localhost:8080/api/jobs/550e.../clips/01_Funny_Moment_funny_9x16.mp4
```

### `GET /api/jobs/:id/transcript` — Транскрипт

### `DELETE /api/jobs/:id` — Удалить

### `GET /health`

## Полный пример с WebSocket

```javascript
// Upload
const form = new FormData();
form.append('file', videoFile);
form.append('language', 'ru');
form.append('vertical_crop', 'true');

const { id } = await fetch('/api/jobs', { method: 'POST', body: form }).then(r => r.json());

// Live progress
const ws = new WebSocket(`ws://${location.host}/api/jobs/${id}/ws`);
ws.onmessage = ({ data }) => {
  const { status, progress, message } = JSON.parse(data);
  updateUI(status, progress, message);
};
ws.onclose = async () => {
  const job = await fetch(`/api/jobs/${id}`).then(r => r.json());
  renderClips(job.clips);
};
```

## Очередь задач

При `MAX_CONCURRENT_JOBS=2`:
- Первые 2 задачи стартуют сразу
- Следующие ждут в очереди
- Как только слот освобождается — берётся следующая задача
- Через WebSocket видно `queued` → `extracting_audio` когда задача реально начинается

## Vertical crop 9:16

Когда `vertical_crop=true`, FFmpeg применяет:
```
crop=ih*9/16:ih:(iw-ih*9/16)/2:0,scale=1080:1920
```
- Центрированный кроп по горизонтали
- Финальный размер 1080×1920
- Идеально для YouTube Shorts, Instagram Reels, TikTok

## Структура

```
src/
├── main.rs              # Axum + queue worker
├── config.rs            # CLI/env
├── db.rs                # SQLite CRUD
├── error.rs             # API errors
├── models.rs            # Job, Highlight, ProgressEvent, QueuedJob
├── state.rs             # AppState (DB + broadcast + semaphore)
├── routes/
│   ├── health.rs
│   └── jobs.rs          # CRUD + WS endpoint
└── services/
    ├── ffmpeg.rs         # + vertical crop
    ├── transcribe.rs     # OpenRouter STT
    ├── analyzer.rs       # OpenRouter highlights
    └── pipeline.rs       # Orchestrator + WS progress
```

## Docker

```dockerfile
FROM rust:1.75-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ffmpeg ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/video-clipper /usr/local/bin/
ENV DATA_DIR=/data
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["video-clipper"]
```

```yaml
# docker-compose.yml
services:
  clipper:
    build: .
    ports: ["8080:8080"]
    environment:
      - OPENROUTER_API_KEY=${OPENROUTER_API_KEY}
      - MAX_CONCURRENT_JOBS=3
      - STT_MODEL=google/gemini-2.5-flash
    volumes: [clipper-data:/data]
volumes:
  clipper-data:
```
