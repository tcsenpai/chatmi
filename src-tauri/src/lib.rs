// chatmi core: SQLite persistence + Node sidecar bridge.
// The sidecar streams MiMo output; we forward each chunk to the webview as an
// event and persist the finished assistant message to SQLite on "done".
use std::sync::Mutex;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

// ---------- data model ----------

#[derive(Serialize, Deserialize, Clone)]
struct ChatMessage {
    id: String,
    conversation_id: String,
    role: String,    // "user" | "assistant"
    content: String,
    reasoning: String,
    created_at: i64,
}

#[derive(Serialize, Clone)]
struct Conversation {
    id: String,
    title: String,
    created_at: i64,
}

// ---------- app state ----------

struct AppState {
    db: Mutex<Connection>,
    sidecar: Mutex<Option<CommandChild>>,
    // Accumulates in-flight assistant streams by message id → (conversation_id, content, reasoning).
    pending: Mutex<std::collections::HashMap<String, (String, String, String)>>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn init_db(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS conversations (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            reasoning TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL
        );",
    )
    .expect("db init");
}

// ---------- commands ----------

#[tauri::command]
fn list_conversations(state: State<AppState>) -> Result<Vec<Conversation>, String> {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT id, title, created_at FROM conversations ORDER BY created_at DESC")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Conversation {
                id: r.get(0)?,
                title: r.get(1)?,
                created_at: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

#[tauri::command]
fn new_conversation(state: State<AppState>) -> Result<Conversation, String> {
    let conv = Conversation {
        id: uuid::Uuid::new_v4().to_string(),
        title: "New chat".to_string(),
        created_at: now(),
    };
    let db = state.db.lock().unwrap();
    db.execute(
        "INSERT INTO conversations (id, title, created_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![conv.id, conv.title, conv.created_at],
    )
    .map_err(|e| e.to_string())?;
    Ok(conv)
}

#[tauri::command]
fn delete_conversation(state: State<AppState>, id: String) -> Result<(), String> {
    let db = state.db.lock().unwrap();
    db.execute("DELETE FROM messages WHERE conversation_id = ?1", [&id])
        .map_err(|e| e.to_string())?;
    db.execute("DELETE FROM conversations WHERE id = ?1", [&id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_messages(state: State<AppState>, conversation_id: String) -> Result<Vec<ChatMessage>, String> {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare(
            "SELECT id, conversation_id, role, content, reasoning, created_at
             FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([&conversation_id], |r| {
            Ok(ChatMessage {
                id: r.get(0)?,
                conversation_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                reasoning: r.get(4)?,
                created_at: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

/// Persist the user message, then ask the sidecar to stream an assistant reply.
#[derive(Deserialize)]
struct Attachment {
    kind: String, // "image" | "audio"
    mime: String, // e.g. "image/png", "audio/wav"
    data: String, // raw base64 (no data-URI prefix)
}

/// Build one MiMo content part from an attachment, or None if unknown kind.
fn attachment_part(a: &Attachment) -> Option<serde_json::Value> {
    match a.kind.as_str() {
        "image" => Some(serde_json::json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", a.mime, a.data) }
        })),
        "audio" => {
            let format = if a.mime.contains("mp3") { "mp3" } else { "wav" };
            Some(serde_json::json!({
                "type": "input_audio",
                "input_audio": { "data": a.data, "format": format }
            }))
        }
        _ => None,
    }
}

/// Returns the assistant message id so the frontend can key its streaming updates.
/// When `attachments` is non-empty the user message is stored as a JSON content
/// array (text + image_url/input_audio parts) so reload+resend stay correct.
#[tauri::command]
fn send_message(
    state: State<AppState>,
    conversation_id: String,
    content: String,
    attachments: Option<Vec<Attachment>>,
) -> Result<String, String> {
    let attachments = attachments.unwrap_or_default();

    // The stored content is plain text when there are no attachments, otherwise
    // a JSON string of the MiMo content-array.
    let stored_content: String = if attachments.is_empty() {
        content.clone()
    } else {
        let mut parts: Vec<serde_json::Value> = Vec::new();
        if !content.is_empty() {
            parts.push(serde_json::json!({ "type": "text", "text": content }));
        }
        for a in &attachments {
            if let Some(p) = attachment_part(a) {
                parts.push(p);
            }
        }
        serde_json::Value::Array(parts).to_string()
    };

    let user_msg = ChatMessage {
        id: uuid::Uuid::new_v4().to_string(),
        conversation_id: conversation_id.clone(),
        role: "user".to_string(),
        content: stored_content,
        reasoning: String::new(),
        created_at: now(),
    };

    // Build the full history to send, and persist the user message + title.
    let history: Vec<serde_json::Value> = {
        let db = state.db.lock().unwrap();
        db.execute(
            "INSERT INTO messages (id, conversation_id, role, content, reasoning, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                user_msg.id,
                user_msg.conversation_id,
                user_msg.role,
                user_msg.content,
                user_msg.reasoning,
                user_msg.created_at
            ],
        )
        .map_err(|e| e.to_string())?;

        // First user message becomes the conversation title (trimmed).
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND role = 'user'",
                [&conversation_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if count == 1 {
            let title: String = content.chars().take(48).collect();
            let _ = db.execute(
                "UPDATE conversations SET title = ?1 WHERE id = ?2",
                rusqlite::params![title, conversation_id],
            );
        }

        let mut stmt = db
            .prepare(
                "SELECT role, content FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([&conversation_id], |r| {
                let role: String = r.get(0)?;
                let content: String = r.get(1)?;
                // A stored content that parses as a JSON array is a multimodal
                // content-array; send it as-is. Otherwise it's plain text.
                let content_val: serde_json::Value = serde_json::from_str(&content)
                    .ok()
                    .filter(|v: &serde_json::Value| v.is_array())
                    .unwrap_or(serde_json::Value::String(content));
                Ok(serde_json::json!({ "role": role, "content": content_val }))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?
    };

    // Reserve the assistant message id and register it as pending.
    let assistant_id = uuid::Uuid::new_v4().to_string();
    state.pending.lock().unwrap().insert(
        assistant_id.clone(),
        (conversation_id.clone(), String::new(), String::new()),
    );

    // Write the request to the sidecar's stdin.
    let req = serde_json::json!({ "id": assistant_id, "messages": history });
    let line = format!("{}\n", req);
    {
        let mut guard = state.sidecar.lock().unwrap();
        let child = guard.as_mut().ok_or("sidecar not running")?;
        child
            .write(line.as_bytes())
            .map_err(|e| format!("sidecar write failed: {e}"))?;
    }

    Ok(assistant_id)
}

/// Open a native file picker and return the chosen image/audio file as a
/// base64 attachment ready to embed in a message. None if the user cancels.
#[tauri::command]
async fn pick_attachment(app: AppHandle) -> Result<Option<serde_json::Value>, String> {
    use base64::Engine;
    use tauri_plugin_dialog::DialogExt;

    let file = app
        .dialog()
        .file()
        .add_filter("Images & audio", &["png", "jpg", "jpeg", "webp", "gif", "wav", "mp3"])
        .blocking_pick_file();

    let Some(path) = file else { return Ok(None) };
    let path = path.into_path().map_err(|e| e.to_string())?;
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let (kind, mime) = match ext.as_str() {
        "png" => ("image", "image/png"),
        "jpg" | "jpeg" => ("image", "image/jpeg"),
        "webp" => ("image", "image/webp"),
        "gif" => ("image", "image/gif"),
        "wav" => ("audio", "audio/wav"),
        "mp3" => ("audio", "audio/mpeg"),
        _ => return Err(format!("unsupported file type: {ext}")),
    };

    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(Some(serde_json::json!({
        "kind": kind,
        "mime": mime,
        "data": data,
        "name": path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
    })))
}

// ---------- sidecar lifecycle ----------

#[derive(Deserialize)]
struct Chunk {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    message: String,
}

fn spawn_sidecar(app: &AppHandle) {
    // Load .env so the API key reaches the sidecar without living in the webview.
    let _ = dotenvy::dotenv();
    let api_key = std::env::var("MIMO_API_KEY").unwrap_or_default();
    let mimo_url = std::env::var("MIMO_URL").unwrap_or_default();

    let sidecar = app
        .shell()
        .sidecar("chatmi-sidecar")
        .expect("sidecar binary")
        .env("MIMO_API_KEY", api_key)
        .env("MIMO_URL", mimo_url);

    let (mut rx, child) = sidecar.spawn().expect("spawn sidecar");
    app.state::<AppState>()
        .sidecar
        .lock()
        .unwrap()
        .replace(child);

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let CommandEvent::Stdout(bytes) = event {
                let text = String::from_utf8_lossy(&bytes);
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(chunk) = serde_json::from_str::<Chunk>(line) {
                        handle_chunk(&handle, chunk);
                    }
                }
            }
        }
    });
}

fn handle_chunk(app: &AppHandle, chunk: Chunk) {
    let state = app.state::<AppState>();
    match chunk.kind.as_str() {
        "reasoning" | "content" | "text" => {
            {
                let mut pending = state.pending.lock().unwrap();
                if let Some((_conv, content, reasoning)) = pending.get_mut(&chunk.id) {
                    if chunk.kind == "reasoning" {
                        reasoning.push_str(&chunk.text);
                    } else {
                        content.push_str(&chunk.text);
                    }
                }
            }
            let _ = app.emit(
                "mimo-chunk",
                serde_json::json!({ "id": chunk.id, "kind": chunk.kind, "text": chunk.text }),
            );
        }
        "done" => {
            let finished = state.pending.lock().unwrap().remove(&chunk.id);
            if let Some((conv, content, reasoning)) = finished {
                let db = state.db.lock().unwrap();
                let _ = db.execute(
                    "INSERT INTO messages (id, conversation_id, role, content, reasoning, created_at)
                     VALUES (?1, ?2, 'assistant', ?3, ?4, ?5)",
                    rusqlite::params![chunk.id, conv, content, reasoning, now()],
                );
            }
            let _ = app.emit("mimo-done", serde_json::json!({ "id": chunk.id }));
        }
        "error" => {
            state.pending.lock().unwrap().remove(&chunk.id);
            let _ = app.emit(
                "mimo-error",
                serde_json::json!({ "id": chunk.id, "message": chunk.message }),
            );
        }
        _ => {}
    }
}

// ---------- entry ----------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let db_path = app
                .path()
                .app_data_dir()
                .expect("app data dir")
                .join("chatmi.db");
            std::fs::create_dir_all(db_path.parent().unwrap()).ok();
            let conn = Connection::open(&db_path).expect("open db");
            init_db(&conn);

            app.manage(AppState {
                db: Mutex::new(conn),
                sidecar: Mutex::new(None),
                pending: Mutex::new(std::collections::HashMap::new()),
            });

            spawn_sidecar(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_conversations,
            new_conversation,
            delete_conversation,
            get_messages,
            send_message,
            pick_attachment
        ])
        .run(tauri::generate_context!())
        .expect("error while running chatmi");
}
