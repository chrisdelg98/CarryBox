mod remote;

use remote::{CancelHandle, ConnConfig, FtpSource, RemoteEntry, RemoteSource, SftpSource};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

/// Conexion remota activa (la del modo Descargar). `None` mientras no se conecta.
/// Se guarda detras de Arc<Mutex> para compartirla entre los comandos async,
/// que ejecutan el trabajo bloqueante de la red en `spawn_blocking`.
#[derive(Default)]
struct AppState {
    source: Arc<Mutex<Option<Box<dyn RemoteSource>>>>,
    /// Bandera para cancelar la transferencia en curso.
    cancel: Arc<AtomicBool>,
    /// Handle para cerrar el socket de datos y abortar al instante un read colgado.
    cancel_handle: CancelHandle,
    /// Ultima configuracion de conexion (en memoria), para reconectar tras una caida.
    last_config: Arc<Mutex<Option<ConnConfig>>>,
}

/// Crea el proveedor remoto adecuado segun el protocolo.
fn make_source(cfg: &ConnConfig, cancel: &CancelHandle) -> Result<Box<dyn RemoteSource>, String> {
    match cfg.protocol.as_str() {
        "ftp" | "ftps" => Ok(Box::new(FtpSource::new(cancel.clone()))),
        "sftp" => Ok(Box::new(SftpSource::new()?)),
        other => Err(format!("Protocolo no soportado: {other}")),
    }
}

/// Reemplaza la conexion por una nueva y limpia. Se usa tras una transferencia
/// interrumpida: en FTP, abortar a mitad deja el canal de control con respuestas
/// pendientes (p.ej. un 226) que rompen el siguiente comando.
fn reconnect_source(
    src: &mut Box<dyn RemoteSource>,
    config: &ConnConfig,
    cancel: &CancelHandle,
    app: &AppHandle,
) {
    let _ = src.disconnect();
    if let Ok(ns) = make_source(config, cancel) {
        *src = ns;
        match src.connect(config) {
            Ok(()) => emit(app, "status", "Conexion restablecida."),
            Err(e) => emit(app, "error", format!("No se pudo reconectar: {e}")),
        }
    }
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
#[serde(rename_all = "camelCase")]
struct TransferProgress {
    /// "download" | "upload"
    kind: String,
    /// "progress" | "done" | "error" | "cancelled"
    state: String,
    /// Archivo actual.
    name: String,
    transferred: u64,
    total: u64,
    // --- Avance general de TODO el lote ---
    overall_transferred: u64,
    overall_total: u64,
    files_done: u64,
    total_files: u64,
}

fn join_remote(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Contexto de una transferencia en curso (lote): lleva los totales generales,
/// la bandera de cancelacion y emite el progreso a la UI con throttle.
struct Xfer {
    app: AppHandle,
    kind: &'static str,
    cancel: Arc<AtomicBool>,
    cancel_handle: CancelHandle,
    total_files: u64,
    total_bytes: u64,
    files_done: u64,
    bytes_done: u64, // bytes de archivos ya completados
    last_emit: Instant,
}

impl Xfer {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn emit(&self, state: &str, name: &str, cur: u64, cur_total: u64) {
        let _ = self.app.emit(
            "transfer",
            TransferProgress {
                kind: self.kind.into(),
                state: state.into(),
                name: name.into(),
                transferred: cur,
                total: cur_total,
                overall_transferred: self.bytes_done + cur,
                overall_total: self.total_bytes,
                files_done: self.files_done,
                total_files: self.total_files,
            },
        );
    }
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
    state.cancel.store(false, Ordering::Relaxed);
    let cfg_store = state.last_config.clone();
    let cancel_h = state.cancel_handle.clone();
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
        let mut src = make_source(&config, &cancel_h)?;
        match src.connect(&config) {
            Ok(()) => emit(&app, "status", "Sesion iniciada (login correcto)."),
            Err(e) => {
                emit(&app, "error", e.clone());
                return Err(e);
            }
        }
        let cwd = src.pwd().unwrap_or_else(|_| "/".to_string());
        emit(&app, "status", format!("Carpeta inicial: {cwd}"));
        // Recordar la config en memoria para reconectar si se cae.
        if let Ok(mut g) = cfg_store.lock() {
            *g = Some(config.clone());
        }
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
    let cancel = state.cancel.clone();
    let cancel_h = state.cancel_handle.clone();
    // Si hay una transferencia colgada, pedir cancelacion y cerrar su socket de datos.
    cancel.store(true, Ordering::Relaxed);
    cancel_h.shutdown();
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

/// Cuenta archivos y bytes de un arbol remoto (para el avance general).
fn count_remote(
    src: &mut Box<dyn RemoteSource>,
    path: &str,
    is_dir: bool,
    size: u64,
) -> Result<(u64, u64), String> {
    if is_dir {
        let (mut f, mut b) = (0u64, 0u64);
        for e in src.list_dir(path)? {
            let (cf, cb) = count_remote(src, &join_remote(path, &e.name), e.is_dir, e.size)?;
            f += cf;
            b += cb;
        }
        Ok((f, b))
    } else {
        Ok((1, size))
    }
}

/// Cuenta archivos y bytes de un arbol local.
fn count_local(path: &Path, is_dir: bool, size: u64) -> (u64, u64) {
    if is_dir {
        let (mut f, mut b) = (0u64, 0u64);
        if let Ok(rd) = std::fs::read_dir(path) {
            for entry in rd.flatten() {
                let p = entry.path();
                let isd = p.is_dir();
                let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let (cf, cb) = count_local(&p, isd, sz);
                f += cf;
                b += cb;
            }
        }
        (f, b)
    } else {
        (1, size)
    }
}

#[derive(Clone, Copy)]
enum ConflictPolicy {
    Replace,
    Skip,
    Newer,
    KeepBoth,
}

impl ConflictPolicy {
    fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "skip" => Self::Skip,
            "newer" => Self::Newer,
            "keep_both" | "keep-both" | "both" | "rename" => Self::KeepBoth,
            _ => Self::Replace,
        }
    }
}

fn split_remote_path(path: &str) -> (String, String) {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return ("/".to_string(), "".to_string());
    }
    if let Some(idx) = trimmed.rfind('/') {
        let parent = if idx == 0 {
            "/".to_string()
        } else {
            trimmed[..idx].to_string()
        };
        let name = trimmed[idx + 1..].to_string();
        (parent, name)
    } else {
        ("/".to_string(), trimmed.to_string())
    }
}

fn remote_find_entry(
    src: &mut Box<dyn RemoteSource>,
    path: &str,
) -> Result<Option<RemoteEntry>, String> {
    let (parent, name) = split_remote_path(path);
    if name.is_empty() {
        return Ok(None);
    }
    for e in src.list_dir(&parent)? {
        if e.name == name {
            return Ok(Some(e));
        }
    }
    Ok(None)
}

fn local_modified_epoch(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn unique_remote_path(
    src: &mut Box<dyn RemoteSource>,
    original: &str,
    is_dir: bool,
) -> Result<String, String> {
    let (parent, name) = split_remote_path(original);
    let (stem, ext) = if is_dir {
        (name.clone(), String::new())
    } else if let Some((a, b)) = name.rsplit_once('.') {
        if a.is_empty() || b.is_empty() {
            (name.clone(), String::new())
        } else {
            (a.to_string(), b.to_string())
        }
    } else {
        (name.clone(), String::new())
    };

    let mut i: u32 = 1;
    loop {
        let candidate_name = if is_dir || ext.is_empty() {
            format!("{stem} ({i})")
        } else {
            format!("{stem} ({i}).{ext}")
        };
        let candidate = join_remote(&parent, &candidate_name);
        if remote_find_entry(src, &candidate)?.is_none() {
            return Ok(candidate);
        }
        i += 1;
        if i > 9999 {
            return Err("No se pudo generar un nombre unico remoto".to_string());
        }
    }
}

/// Decide si un error vale la pena reintentar (caidas de conexion), nunca una
/// cancelacion del usuario.
fn is_retryable(e: &str) -> bool {
    let el = e.to_lowercase();
    if el.contains("cancelado") {
        return false;
    }
    el.contains("10054")
        || el.contains("10053")
        || el.contains("10060")
        || el.contains("forcibly closed")
        || el.contains("reset")
        || el.contains("broken pipe")
        || el.contains("connection")
        || el.contains("timed out")
        || el.contains("eof")
        || el.contains("not connected")
}

/// Transfiere UN archivo con reintentos + reconexion + reanudacion (descarga).
#[allow(clippy::too_many_arguments)]
fn transfer_file(
    src: &mut Box<dyn RemoteSource>,
    config: &ConnConfig,
    ctx: &mut Xfer,
    is_upload: bool,
    local: &Path,
    remote: &str,
    name: &str,
    size: u64,
) -> Result<(), String> {
    const MAX: u32 = 4;
    let mut attempt = 0u32;
    let mut fresh = false; // true = descargar desde cero (descartar parcial)
    // En algunos servidores FTP/FTPS, REST+RETR queda colgado tras cancelaciones.
    // Mantener resume solo en SFTP evita este bloqueo.
    let resume_supported = !is_upload && config.protocol.eq_ignore_ascii_case("sftp");
    loop {
        if ctx.cancelled() {
            return Err("Cancelado".to_string());
        }
        // Reanudacion (solo descarga): cuantos bytes hay ya en disco.
        let resume = if is_upload {
            0
        } else {
            let have = std::fs::metadata(local).map(|m| m.len()).unwrap_or(0);
            if size > 0 && have == size {
                return Ok(()); // ya completo -> saltar
            }
            if !resume_supported {
                // FTP/FTPS: reiniciar desde cero para evitar cuelgue en REST.
                if have > 0 {
                    let _ = std::fs::remove_file(local);
                }
                0
            } else if fresh {
                let _ = std::fs::remove_file(local); // descartar parcial y rebajar completo
                0
            } else if have > 0 && (size == 0 || have < size) {
                have
            } else {
                0
            }
        };
        ctx.emit("progress", name, resume, size);

        let res = {
            let mut cb = |t: u64| -> bool {
                if ctx.cancelled() {
                    return false;
                }
                if ctx.last_emit.elapsed() >= Duration::from_millis(80) {
                    ctx.last_emit = Instant::now();
                    ctx.emit("progress", name, t, size);
                }
                true
            };
            if is_upload {
                src.upload_file(local, remote, resume, &mut cb)
            } else {
                src.download_file(remote, local, resume, &mut cb)
            }
        };

        match res {
            Ok(()) => return Ok(()),
            Err(e) => {
                if ctx.cancelled() {
                    return Err("Cancelado".to_string());
                }
                if attempt >= MAX || !is_retryable(&e) {
                    return Err(e);
                }
                attempt += 1;
                // Si fallo una descarga REANUDADA, el proximo intento va desde cero
                // (reemplaza el archivo): algunos servers tienen el REST/resume roto.
                if !is_upload && resume > 0 && !fresh {
                    fresh = true;
                    emit(
                        &ctx.app,
                        "status",
                        format!("Reanudacion fallida en \"{name}\"; se descargara completo de nuevo."),
                    );
                }
                emit(
                    &ctx.app,
                    "status",
                    format!("Conexion perdida ({e}). Reintentando {attempt}/{MAX}..."),
                );
                let _ = src.disconnect();
                std::thread::sleep(Duration::from_millis(700 * attempt as u64));
                match make_source(config, &ctx.cancel_handle) {
                    Ok(ns) => *src = ns,
                    Err(me) => return Err(me),
                }
                if let Err(ce) = src.connect(config) {
                    emit(&ctx.app, "status", format!("Reconexion fallida: {ce}"));
                    if attempt >= MAX {
                        return Err(format!("No se pudo reconectar: {ce}"));
                    }
                }
            }
        }
    }
}

fn do_download(
    src: &mut Box<dyn RemoteSource>,
    config: &ConnConfig,
    ctx: &mut Xfer,
    remote_path: &str,
    name: &str,
    is_dir: bool,
    size: u64,
    local_parent: &Path,
) -> Result<(), String> {
    if ctx.cancelled() {
        return Err("Cancelado".to_string());
    }
    let local_target = local_parent.join(name);
    if is_dir {
        std::fs::create_dir_all(&local_target).map_err(|e| e.to_string())?;
        for e in src.list_dir(remote_path)? {
            let child = join_remote(remote_path, &e.name);
            do_download(src, config, ctx, &child, &e.name, e.is_dir, e.size, &local_target)?;
        }
    } else {
        match transfer_file(src, config, ctx, false, &local_target, remote_path, name, size) {
            Ok(()) => {}
            Err(e) => {
                // Cancelar aborta todo; un archivo problematico solo se omite.
                if ctx.cancelled() {
                    return Err(e);
                }
                reconnect_source(src, config, &ctx.cancel_handle, &ctx.app);
                emit(&ctx.app, "error", format!("Omitido \"{name}\": {e}"));
            }
        }
        ctx.bytes_done += size;
        ctx.files_done += 1;
        ctx.emit("progress", name, size, size);
    }
    Ok(())
}

fn do_upload(
    src: &mut Box<dyn RemoteSource>,
    config: &ConnConfig,
    ctx: &mut Xfer,
    policy: ConflictPolicy,
    local_path: &Path,
    name: &str,
    is_dir: bool,
    remote_parent: &str,
) -> Result<(), String> {
    if ctx.cancelled() {
        return Err("Cancelado".to_string());
    }
    let mut remote_target = join_remote(remote_parent, name);
    if is_dir {
        if let Some(existing) = remote_find_entry(src, &remote_target)? {
            if existing.is_dir {
                if let ConflictPolicy::KeepBoth = policy {
                    remote_target = unique_remote_path(src, &remote_target, true)?;
                    let _ = src.mkdir(&remote_target);
                }
            } else {
                match policy {
                    ConflictPolicy::Replace => {
                        let _ = src.delete_file(&remote_target);
                        let _ = src.mkdir(&remote_target);
                    }
                    ConflictPolicy::Skip | ConflictPolicy::Newer => {
                        emit(
                            &ctx.app,
                            "status",
                            format!("Omitido existente: \"{name}\""),
                        );
                        return Ok(());
                    }
                    ConflictPolicy::KeepBoth => {
                        remote_target = unique_remote_path(src, &remote_target, true)?;
                        let _ = src.mkdir(&remote_target);
                    }
                }
            }
        } else {
            let _ = src.mkdir(&remote_target);
        }
        for entry in std::fs::read_dir(local_path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let p = entry.path();
            let n = entry.file_name().to_string_lossy().to_string();
            let isd = p.is_dir();
            do_upload(src, config, ctx, policy, &p, &n, isd, &remote_target)?;
        }
    } else {
        let size = std::fs::metadata(local_path).map(|m| m.len()).unwrap_or(0);
        if let Some(existing) = remote_find_entry(src, &remote_target)? {
            match policy {
                ConflictPolicy::Replace => {}
                ConflictPolicy::Skip => {
                    emit(
                        &ctx.app,
                        "status",
                        format!("Omitido existente: \"{name}\""),
                    );
                    ctx.files_done += 1;
                    ctx.bytes_done += size;
                    ctx.emit("progress", name, size, size);
                    return Ok(());
                }
                ConflictPolicy::Newer => {
                    let local_ts = local_modified_epoch(local_path).unwrap_or(0);
                    let remote_ts = existing.modified.unwrap_or(0);
                    if local_ts > 0 && remote_ts > 0 && local_ts <= remote_ts {
                        emit(
                            &ctx.app,
                            "status",
                            format!("Omitido no-mas-nuevo: \"{name}\""),
                        );
                        ctx.files_done += 1;
                        ctx.bytes_done += size;
                        ctx.emit("progress", name, size, size);
                        return Ok(());
                    }
                }
                ConflictPolicy::KeepBoth => {
                    remote_target = unique_remote_path(src, &remote_target, false)?;
                }
            }
        }
        match transfer_file(src, config, ctx, true, local_path, &remote_target, name, size) {
            Ok(()) => {}
            Err(e) => {
                if ctx.cancelled() {
                    return Err(e);
                }
                reconnect_source(src, config, &ctx.cancel_handle, &ctx.app);
                emit(&ctx.app, "error", format!("Omitido \"{name}\": {e}"));
            }
        }
        ctx.bytes_done += size;
        ctx.files_done += 1;
        ctx.emit("progress", name, size, size);
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
    let cancel = state.cancel.clone();
    let cfg_arc = state.last_config.clone();
    let cancel_h = state.cancel_handle.clone();
    cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let config = cfg_arc
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or("No conectado")?;
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        let src = guard.as_mut().ok_or("No conectado")?;
        // Siempre arrancar el lote con una sesion limpia evita estado sucio tras cancelaciones.
        reconnect_source(src, &config, &cancel_h, &app);
        emit(&app, "status", "Calculando total...");
        let (mut tf, mut tb) = (0u64, 0u64);
        for it in &items {
            let (f, b) = count_remote(src, &it.path, it.is_dir, it.size)?;
            tf += f;
            tb += b;
        }
        let mut ctx = Xfer {
            app: app.clone(),
            kind: "download",
            cancel: cancel.clone(),
            cancel_handle: cancel_h.clone(),
            total_files: tf,
            total_bytes: tb,
            files_done: 0,
            bytes_done: 0,
            last_emit: Instant::now(),
        };
        let base = PathBuf::from(&local_dir);
        for it in &items {
            emit(&app, "status", format!("Descargando: {}", it.name));
            if let Err(e) =
                do_download(src, &config, &mut ctx, &it.path, &it.name, it.is_dir, it.size, &base)
            {
                // Interrumpir deja el control FTP sucio -> reconectar para limpiar.
                reconnect_source(src, &config, &cancel_h, &app);
                if cancel.load(Ordering::Relaxed) {
                    emit(&app, "status", "Transferencia cancelada.");
                    ctx.emit("cancelled", "", 0, 0);
                    return Ok(());
                }
                emit(&app, "error", e.clone());
                ctx.emit("error", &it.name, 0, 0);
                return Err(e);
            }
        }
        emit(&app, "status", "Descarga completada.");
        ctx.emit("done", "", 0, 0);
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
    conflict_policy: Option<String>,
) -> Result<(), String> {
    let arc = state.source.clone();
    let cancel = state.cancel.clone();
    let cfg_arc = state.last_config.clone();
    let cancel_h = state.cancel_handle.clone();
    let policy = conflict_policy
        .as_deref()
        .map(ConflictPolicy::from_str)
        .unwrap_or(ConflictPolicy::Replace);
    cancel.store(false, Ordering::Relaxed);
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let config = cfg_arc
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or("No conectado")?;
        let mut guard = arc.lock().map_err(|_| "estado bloqueado".to_string())?;
        let src = guard.as_mut().ok_or("No conectado")?;
        // Siempre arrancar el lote con una sesion limpia evita estado sucio tras cancelaciones.
        reconnect_source(src, &config, &cancel_h, &app);
        let (mut tf, mut tb) = (0u64, 0u64);
        for it in &items {
            let lp = PathBuf::from(&it.path);
            let (f, b) = count_local(&lp, it.is_dir, it.size);
            tf += f;
            tb += b;
        }
        let mut ctx = Xfer {
            app: app.clone(),
            kind: "upload",
            cancel: cancel.clone(),
            cancel_handle: cancel_h.clone(),
            total_files: tf,
            total_bytes: tb,
            files_done: 0,
            bytes_done: 0,
            last_emit: Instant::now(),
        };
        for it in &items {
            emit(&app, "status", format!("Subiendo: {}", it.name));
            let lp = PathBuf::from(&it.path);
            if let Err(e) = do_upload(
                src,
                &config,
                &mut ctx,
                policy,
                &lp,
                &it.name,
                it.is_dir,
                &remote_dir,
            ) {
                reconnect_source(src, &config, &cancel_h, &app);
                if cancel.load(Ordering::Relaxed) {
                    emit(&app, "status", "Transferencia cancelada.");
                    ctx.emit("cancelled", "", 0, 0);
                    return Ok(());
                }
                emit(&app, "error", e.clone());
                ctx.emit("error", &it.name, 0, 0);
                return Err(e);
            }
        }
        emit(&app, "status", "Subida completada.");
        ctx.emit("done", "", 0, 0);
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

/// Cancelar la transferencia en curso (la bandera la leen los bucles de copia).
#[tauri::command]
fn cancel_transfer(app: AppHandle, state: State<'_, AppState>) {
    emit(&app, "status", "Cancelando transferencia...");
    state.cancel.store(true, Ordering::Relaxed);
    // Cerrar el socket de datos en curso para desbloquear al instante un read colgado.
    state.cancel_handle.shutdown();
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
            cancel_transfer,
            list_local,
            local_mkdir,
            local_rename,
            local_delete
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
