mod remote;

use remote::{ConnConfig, FtpSource, RemoteEntry, RemoteSource, SftpSource};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

/// Conexion remota activa (la del modo Descargar). `None` mientras no se conecta.
/// Se guarda detras de Arc<Mutex> para compartirla entre los comandos async,
/// que ejecutan el trabajo bloqueante de la red en `spawn_blocking`.
#[derive(Default)]
struct AppState {
    source: Arc<Mutex<Option<Box<dyn RemoteSource>>>>,
}

// ---------------------------------------------------------------------------
// Log en vivo hacia la UI (estilo FileZilla: Estado / Comando / Respuesta)
// ---------------------------------------------------------------------------

/// Una linea de log que viaja al frontend por el evento "ftp-log".
#[derive(Clone, Serialize)]
struct LogLine {
    /// "status" | "command" | "response" | "error"
    kind: String,
    text: String,
}

fn emit(app: &AppHandle, kind: &str, text: impl Into<String>) {
    let _ = app.emit(
        "ftp-log",
        LogLine {
            kind: kind.into(),
            text: text.into(),
        },
    );
}

/// Logger que reenvia lo que suppaftp registra (comandos FTP y respuestas del
/// server) hacia la UI. Asi el usuario ve EXACTAMENTE lo que pasa en el protocolo.
struct EventLogger {
    app: Mutex<Option<AppHandle>>,
}

static LOGGER: EventLogger = EventLogger {
    app: Mutex::new(None),
};

impl log::Log for EventLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // Solo nos interesa el trafico del cliente FTP.
        metadata.target().starts_with("suppaftp")
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let guard = match self.app.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(app) = guard.as_ref() {
            let msg = format!("{}", record.args());
            // suppaftp registra los comandos como "CMD <texto>".
            let (kind, text) = if let Some(rest) = msg.strip_prefix("CMD ") {
                ("command", rest.to_string())
            } else {
                ("response", msg)
            };
            emit(app, kind, text);
        }
    }

    fn flush(&self) {}
}

// ---------------------------------------------------------------------------
// Transferencias (descargar / subir) con progreso hacia la UI
// ---------------------------------------------------------------------------

/// Un item a transferir, tal como lo envia la UI.
#[derive(Clone, Deserialize)]
struct TransferItem {
    name: String,
    /// Ruta completa: remota (download) o local (upload).
    path: String,
    is_dir: bool,
    #[serde(default)]
    size: u64,
}

/// Progreso de transferencia que viaja al frontend por el evento "transfer".
#[derive(Clone, Serialize)]
struct TransferProgress {
    /// "download" | "upload"
    kind: String,
    /// "progress" | "done" | "error"
    state: String,
    /// Archivo actual.
    name: String,
    transferred: u64,
    total: u64,
}

fn join_remote(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

fn emit_progress(app: &AppHandle, kind: &str, state: &str, name: &str, transferred: u64, total: u64) {
    let _ = app.emit(
        "transfer",
        TransferProgress {
            kind: kind.into(),
            state: state.into(),
            name: name.into(),
            transferred,
            total,
        },
    );
}

// ---------------------------------------------------------------------------
// Comandos: lado REMOTO (panel derecho del FileZilla)
// ---------------------------------------------------------------------------

#[tauri::command]
async fn remote_connect(
    app: AppHandle,
    state: State<'_, AppState>,
    config: ConnConfig,
) -> Result<String, String> {
    let arc = state.source.clone();
    emit(
        &app,
        "status",
        format!(
            "Conectando a {}:{} por {} ...",
            config.host,
            config.port,
            config.protocol.to_uppercase()
        ),
    );
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let mut src: Box<dyn RemoteSource> = match config.protocol.as_str() {
            "ftp" | "ftps" => Box::new(FtpSource::new()),
            "sftp" => Box::new(SftpSource::new()?),
            other => return Err(format!("Protocolo no soportado: {other}")),
        };
        match src.connect(&config) {
            Ok(()) => emit(&app, "status", "Sesion iniciada (login correcto)."),
            Err(e) => {
                emit(&app, "error", e.clone());
                return Err(e);
            }
        }
        let cwd = src.pwd().unwrap_or_else(|_| "/".to_string());
        emit(&app, "status", format!("Carpeta inicial: {cwd}"));
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        if let Some(mut old) = guard.take() {
            let _ = old.disconnect();
        }
        *guard = Some(src);
        Ok(cwd)
    })
    .await
    .map_err(|e| format!("tarea fallo: {e}"))?
}

#[tauri::command]
async fn remote_list(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<Vec<RemoteEntry>, String> {
    let arc = state.source.clone();
    emit(&app, "status", format!("Listando {path} ..."));
    tauri::async_runtime::spawn_blocking(move || -> Result<Vec<RemoteEntry>, String> {
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        let src = guard.as_mut().ok_or("No conectado")?;
        match src.list_dir(&path) {
            Ok(entries) => {
                emit(
                    &app,
                    "status",
                    format!("Listado correcto: {} elementos.", entries.len()),
                );
                Ok(entries)
            }
            Err(e) => {
                emit(&app, "error", e.clone());
                Err(e)
            }
        }
    })
    .await
    .map_err(|e| format!("tarea fallo: {e}"))?
}

#[tauri::command]
async fn remote_disconnect(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let arc = state.source.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        if let Some(mut src) = guard.take() {
            let _ = src.disconnect();
        }
        emit(&app, "status", "Desconectado del servidor.");
        Ok(())
    })
    .await
    .map_err(|e| format!("tarea fallo: {e}"))?
}

// --- Recursion (bajar/subir/borrar carpetas enteras) usando el trait ---

fn do_download(
    src: &mut Box<dyn RemoteSource>,
    app: &AppHandle,
    remote_path: &str,
    name: &str,
    is_dir: bool,
    size: u64,
    local_parent: &Path,
) -> Result<(), String> {
    let local_target = local_parent.join(name);
    if is_dir {
        std::fs::create_dir_all(&local_target).map_err(|e| e.to_string())?;
        for e in src.list_dir(remote_path)? {
            let child = join_remote(remote_path, &e.name);
            do_download(src, app, &child, &e.name, e.is_dir, e.size, &local_target)?;
        }
    } else {
        emit_progress(app, "download", "progress", name, 0, size);
        let appc = app.clone();
        let namec = name.to_string();
        let mut last = Instant::now();
        let mut cb = move |t: u64| {
            if last.elapsed() >= Duration::from_millis(80) {
                last = Instant::now();
                emit_progress(&appc, "download", "progress", &namec, t, size);
            }
        };
        src.download_file(remote_path, &local_target, &mut cb)?;
        emit_progress(app, "download", "progress", name, size, size);
    }
    Ok(())
}

fn do_upload(
    src: &mut Box<dyn RemoteSource>,
    app: &AppHandle,
    local_path: &Path,
    name: &str,
    is_dir: bool,
    remote_parent: &str,
) -> Result<(), String> {
    let remote_target = join_remote(remote_parent, name);
    if is_dir {
        let _ = src.mkdir(&remote_target); // si ya existe, seguimos
        for entry in std::fs::read_dir(local_path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let p = entry.path();
            let n = entry.file_name().to_string_lossy().to_string();
            let isd = p.is_dir();
            do_upload(src, app, &p, &n, isd, &remote_target)?;
        }
    } else {
        let size = std::fs::metadata(local_path).map(|m| m.len()).unwrap_or(0);
        emit_progress(app, "upload", "progress", name, 0, size);
        let appc = app.clone();
        let namec = name.to_string();
        let mut last = Instant::now();
        let mut cb = move |t: u64| {
            if last.elapsed() >= Duration::from_millis(80) {
                last = Instant::now();
                emit_progress(&appc, "upload", "progress", &namec, t, size);
            }
        };
        src.upload_file(local_path, &remote_target, &mut cb)?;
        emit_progress(app, "upload", "progress", name, size, size);
    }
    Ok(())
}

fn do_delete(src: &mut Box<dyn RemoteSource>, path: &str, is_dir: bool) -> Result<(), String> {
    if is_dir {
        for e in src.list_dir(path)? {
            let child = join_remote(path, &e.name);
            do_delete(src, &child, e.is_dir)?;
        }
        src.delete_dir(path)
    } else {
        src.delete_file(path)
    }
}

#[tauri::command]
async fn remote_download(
    app: AppHandle,
    state: State<'_, AppState>,
    items: Vec<TransferItem>,
    local_dir: String,
) -> Result<(), String> {
    let arc = state.source.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        let src = guard.as_mut().ok_or("No conectado")?;
        let base = PathBuf::from(&local_dir);
        for it in &items {
            emit(&app, "status", format!("Descargando: {}", it.name));
            if let Err(e) = do_download(src, &app, &it.path, &it.name, it.is_dir, it.size, &base) {
                emit(&app, "error", e.clone());
                emit_progress(&app, "download", "error", &it.name, 0, 0);
                return Err(e);
            }
        }
        emit(&app, "status", "Descarga completada.");
        emit_progress(&app, "download", "done", "", 0, 0);
        Ok(())
    })
    .await
    .map_err(|e| format!("tarea fallo: {e}"))?
}

#[tauri::command]
async fn remote_upload(
    app: AppHandle,
    state: State<'_, AppState>,
    items: Vec<TransferItem>,
    remote_dir: String,
) -> Result<(), String> {
    let arc = state.source.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        let src = guard.as_mut().ok_or("No conectado")?;
        for it in &items {
            emit(&app, "status", format!("Subiendo: {}", it.name));
            let lp = PathBuf::from(&it.path);
            if let Err(e) = do_upload(src, &app, &lp, &it.name, it.is_dir, &remote_dir) {
                emit(&app, "error", e.clone());
                emit_progress(&app, "upload", "error", &it.name, 0, 0);
                return Err(e);
            }
        }
        emit(&app, "status", "Subida completada.");
        emit_progress(&app, "upload", "done", "", 0, 0);
        Ok(())
    })
    .await
    .map_err(|e| format!("tarea fallo: {e}"))?
}

#[tauri::command]
async fn remote_mkdir(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let arc = state.source.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        let src = guard.as_mut().ok_or("No conectado")?;
        src.mkdir(&path).map_err(|e| {
            emit(&app, "error", e.clone());
            e
        })
    })
    .await
    .map_err(|e| format!("tarea fallo: {e}"))?
}

#[tauri::command]
async fn remote_rename(
    app: AppHandle,
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), String> {
    let arc = state.source.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        let src = guard.as_mut().ok_or("No conectado")?;
        src.rename(&from, &to).map_err(|e| {
            emit(&app, "error", e.clone());
            e
        })
    })
    .await
    .map_err(|e| format!("tarea fallo: {e}"))?
}

#[tauri::command]
async fn remote_delete(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
    is_dir: bool,
) -> Result<(), String> {
    let arc = state.source.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        let src = guard.as_mut().ok_or("No conectado")?;
        do_delete(src, &path, is_dir).map_err(|e| {
            emit(&app, "error", e.clone());
            e
        })
    })
    .await
    .map_err(|e| format!("tarea fallo: {e}"))?
}

// ---------------------------------------------------------------------------
// Comandos: lado LOCAL (panel izquierdo del FileZilla)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct LocalEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
}

#[derive(Serialize)]
struct LocalDir {
    path: String,
    parent: Option<String>,
    entries: Vec<LocalEntry>,
}

/// Carpeta del usuario, multiplataforma (Windows: USERPROFILE, *nix: HOME).
fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[tauri::command]
fn list_local(path: Option<String>) -> Result<LocalDir, String> {
    let dir = match path {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => home_dir(),
    };

    let read = std::fs::read_dir(&dir).map_err(|e| format!("No se pudo abrir la carpeta: {e}"))?;
    let mut entries = Vec::new();
    for item in read.flatten() {
        let meta = match item.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let name = item.file_name().to_string_lossy().to_string();
        entries.push(LocalEntry {
            name,
            path: item.path().to_string_lossy().to_string(),
            is_dir: meta.is_dir(),
            size: meta.len(),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    let parent = Path::new(&dir)
        .parent()
        .map(|p| p.to_string_lossy().to_string());

    Ok(LocalDir {
        path: dir.to_string_lossy().to_string(),
        parent,
        entries,
    })
}

#[tauri::command]
fn local_mkdir(parent: String, name: String) -> Result<(), String> {
    let p = PathBuf::from(parent).join(name);
    std::fs::create_dir(&p).map_err(|e| format!("No se pudo crear la carpeta: {e}"))
}

#[tauri::command]
fn local_rename(path: String, new_name: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    let parent = p.parent().ok_or("Ruta invalida")?;
    let dest = parent.join(new_name);
    std::fs::rename(&p, &dest).map_err(|e| format!("No se pudo renombrar: {e}"))
}

#[tauri::command]
fn local_delete(path: String, is_dir: bool) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if is_dir {
        std::fs::remove_dir_all(&p).map_err(|e| format!("No se pudo eliminar la carpeta: {e}"))
    } else {
        std::fs::remove_file(&p).map_err(|e| format!("No se pudo eliminar el archivo: {e}"))
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Capturar el log de suppaftp para reenviarlo a la UI.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            if let Ok(mut g) = LOGGER.app.lock() {
                *g = Some(app.handle().clone());
            }
            // Tamano adaptable: abrir al ~80% de la pantalla (centrada, no
            // maximizada), con limites para que se vea bien en cualquier monitor.
            if let Some(win) = app.get_webview_window("main") {
                if let Ok(Some(monitor)) = win.primary_monitor() {
                    let sz = monitor.size();
                    let w = ((sz.width as f64) * 0.80).round() as u32;
                    let h = ((sz.height as f64) * 0.85).round() as u32;
                    let w = w.clamp(1024, 1600);
                    let h = h.clamp(640, 1000);
                    let _ = win.set_size(tauri::PhysicalSize::new(w, h));
                    let _ = win.center();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            remote_connect,
            remote_list,
            remote_disconnect,
            remote_download,
            remote_upload,
            remote_mkdir,
            remote_rename,
            remote_delete,
            list_local,
            local_mkdir,
            local_rename,
            local_delete
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
