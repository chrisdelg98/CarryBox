//! Modo Subir a S3 — proveedor S3-compatible (aws-sdk-s3).
//!
//! Implementa el mismo `RemoteSource` que FTP/SFTP, de modo que la vista dual y
//! todo el motor de transferencia (progreso, cancelar, reintentos) se reutilizan.
//! En S3 no hay carpetas reales: se emulan con prefijos y delimitador "/".

use crate::remote::{ConnConfig, ProgressFn, RemoteEntry, RemoteSource};
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tokio::runtime::Runtime;

/// Estado de una subida multipart en curso, para poder REANUDAR tras un corte.
#[derive(Serialize, Deserialize)]
struct UploadState {
    bucket: String,
    key: String,
    upload_id: String,
    file_size: u64,
    mtime_secs: u64,
    part_size: u64,
}

/// Ruta del archivo de estado (unos KB) en %APPDATA%\CarryBox\uploads\.
fn upload_state_path(bucket: &str, key: &str) -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    let dir = base.join("CarryBox").join("uploads");
    let _ = std::fs::create_dir_all(&dir);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bucket.hash(&mut h);
    key.hash(&mut h);
    Some(dir.join(format!("{:016x}.json", h.finish())))
}

fn save_upload_state(path: &Path, st: &UploadState) {
    if let Ok(txt) = serde_json::to_string(st) {
        let _ = std::fs::write(path, txt);
    }
}

/// Construye un cliente S3 a partir de la config (sin conectar; es sincrono).
fn build_client(cfg: &ConnConfig) -> Client {
    let creds = Credentials::new(
        cfg.access_key.clone(),
        cfg.secret_key.clone(),
        None,
        None,
        "carrybox",
    );
    let region = if cfg.region.is_empty() {
        "us-east-1".to_string()
    } else {
        cfg.region.clone()
    };
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region))
        .endpoint_url(cfg.endpoint.clone())
        .credentials_provider(creds)
        .force_path_style(cfg.path_style)
        .retry_config(RetryConfig::standard().with_max_attempts(6))
        .build();
    Client::from_conf(conf)
}

/// Info de una subida multipart en progreso (invisible en el bucket hasta completar).
#[derive(Serialize)]
pub struct IncompleteUpload {
    pub key: String,
    pub upload_id: String,
    pub bytes: u64,
    pub parts: u32,
    pub initiated: Option<u64>,
}

async fn sum_parts(client: &Client, bucket: &str, key: &str, upload_id: &str) -> (u32, u64) {
    let (mut count, mut bytes) = (0u32, 0u64);
    let mut marker: Option<String> = None;
    loop {
        let mut req = client.list_parts().bucket(bucket).key(key).upload_id(upload_id);
        if let Some(m) = &marker {
            req = req.part_number_marker(m);
        }
        let out = match req.send().await {
            Ok(o) => o,
            Err(_) => break,
        };
        for p in out.parts() {
            count += 1;
            bytes += p.size().unwrap_or(0).max(0) as u64;
        }
        if out.is_truncated().unwrap_or(false) {
            marker = out.next_part_number_marker().map(|s| s.to_string());
            if marker.is_none() {
                break;
            }
        } else {
            break;
        }
    }
    (count, bytes)
}

/// Lista las subidas multipart en progreso del bucket (con bytes/pedazos ya subidos).
pub fn list_incomplete(cfg: &ConnConfig, rt: &Runtime) -> Result<Vec<IncompleteUpload>, String> {
    let client = build_client(cfg);
    let bucket = cfg.bucket.clone();
    rt.block_on(async move {
        let mut out = Vec::new();
        let mut key_marker: Option<String> = None;
        let mut id_marker: Option<String> = None;
        loop {
            let mut req = client.list_multipart_uploads().bucket(&bucket);
            if let Some(k) = &key_marker {
                req = req.key_marker(k);
            }
            if let Some(i) = &id_marker {
                req = req.upload_id_marker(i);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| format!("No se pudo listar subidas en progreso: {}", s3_err(e)))?;
            for u in resp.uploads() {
                let key = u.key().unwrap_or("").to_string();
                let uid = u.upload_id().unwrap_or("").to_string();
                let initiated = u.initiated().map(|d| d.secs().max(0) as u64);
                let (parts, bytes) = sum_parts(&client, &bucket, &key, &uid).await;
                out.push(IncompleteUpload {
                    key,
                    upload_id: uid,
                    bytes,
                    parts,
                    initiated,
                });
            }
            if resp.is_truncated().unwrap_or(false) {
                key_marker = resp.next_key_marker().map(|s| s.to_string());
                id_marker = resp.next_upload_id_marker().map(|s| s.to_string());
                if key_marker.is_none() && id_marker.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(out)
    })
}

/// Aborta una subida en progreso (S3 borra sus pedazos) y limpia el estado local.
pub fn abort_upload(cfg: &ConnConfig, rt: &Runtime, key: &str, upload_id: &str) -> Result<(), String> {
    let client = build_client(cfg);
    let bucket = cfg.bucket.clone();
    let (k, uid) = (key.to_string(), upload_id.to_string());
    rt.block_on(async move {
        client
            .abort_multipart_upload()
            .bucket(&bucket)
            .key(&k)
            .upload_id(&uid)
            .send()
            .await
            .map_err(|e| format!("No se pudo abortar: {}", s3_err(e)))?;
        Ok::<_, String>(())
    })?;
    if let Some(sp) = upload_state_path(&cfg.bucket, key) {
        let _ = std::fs::remove_file(sp);
    }
    Ok(())
}

/// Aborta TODAS las subidas en progreso del bucket. Devuelve cuantas abortó.
pub fn abort_all(cfg: &ConnConfig, rt: &Runtime) -> Result<usize, String> {
    let list = list_incomplete(cfg, rt)?;
    let mut n = 0;
    for u in &list {
        if abort_upload(cfg, rt, &u.key, &u.upload_id).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// Partes ya subidas segun S3 (fuente de verdad): num -> etag.
async fn list_uploaded_parts(
    client: &Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<HashMap<i32, String>, String> {
    let mut map = HashMap::new();
    let mut marker: Option<String> = None;
    loop {
        let mut req = client.list_parts().bucket(bucket).key(key).upload_id(upload_id);
        if let Some(m) = &marker {
            req = req.part_number_marker(m);
        }
        let out = req.send().await.map_err(|e| s3_err(e))?;
        for p in out.parts() {
            if let Some(n) = p.part_number() {
                map.insert(n, p.e_tag().unwrap_or_default().to_string());
            }
        }
        if out.is_truncated().unwrap_or(false) {
            marker = out.next_part_number_marker().map(|s| s.to_string());
            if marker.is_none() {
                break;
            }
        } else {
            break;
        }
    }
    Ok(map)
}

pub struct S3Source {
    rt: Runtime,
    client: Option<Client>,
    bucket: String,
}

impl S3Source {
    pub fn new() -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("No se pudo crear el runtime async: {e}"))?;
        Ok(Self {
            rt,
            client: None,
            bucket: String::new(),
        })
    }

    fn client(&self) -> Result<Client, String> {
        self.client.clone().ok_or_else(|| "No conectado".to_string())
    }
}

/// Normaliza a prefijo S3: sin "/" inicial, con "/" final si no esta vacio.
fn norm_prefix(path: &str) -> String {
    let p = path.trim_start_matches('/');
    if p.is_empty() {
        String::new()
    } else if p.ends_with('/') {
        p.to_string()
    } else {
        format!("{p}/")
    }
}

/// Mensaje legible de un error de aws-sdk (incluye la cadena de causas).
fn s3_err<E: std::error::Error>(e: E) -> String {
    format!("{}", aws_smithy_types::error::display::DisplayErrorContext(e))
}

impl RemoteSource for S3Source {
    fn connect(&mut self, cfg: &ConnConfig) -> Result<(), String> {
        if cfg.endpoint.is_empty() || cfg.bucket.is_empty() {
            return Err("Faltan el endpoint o el bucket.".to_string());
        }
        let client = build_client(cfg);
        let bucket = cfg.bucket.clone();
        // Pre-flight: valida credenciales + acceso al bucket.
        self.rt.block_on(async {
            client
                .head_bucket()
                .bucket(&bucket)
                .send()
                .await
                .map_err(|e| format!("No se pudo acceder al bucket: {}", s3_err(e)))
        })?;

        self.client = Some(client);
        self.bucket = cfg.bucket.clone();
        Ok(())
    }

    fn pwd(&mut self) -> Result<String, String> {
        Ok("/".to_string())
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let client = self.client()?;
        let bucket = self.bucket.clone();
        let prefix = norm_prefix(path);

        self.rt.block_on(async move {
            let mut entries = Vec::new();
            let mut token: Option<String> = None;
            loop {
                let mut req = client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .delimiter("/")
                    .prefix(&prefix);
                if let Some(t) = &token {
                    req = req.continuation_token(t);
                }
                let out = req
                    .send()
                    .await
                    .map_err(|e| format!("No se pudo listar: {}", s3_err(e)))?;

                // "Carpetas" = common prefixes.
                for cp in out.common_prefixes() {
                    if let Some(p) = cp.prefix() {
                        let name = p.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
                        if !name.is_empty() {
                            entries.push(RemoteEntry {
                                name,
                                is_dir: true,
                                size: 0,
                                modified: None,
                            });
                        }
                    }
                }
                // Archivos = contents.
                for obj in out.contents() {
                    if let Some(key) = obj.key() {
                        if key == prefix || key.ends_with('/') {
                            continue; // marcador de carpeta o el propio prefijo
                        }
                        let name = key.rsplit('/').next().unwrap_or(key).to_string();
                        let size = obj.size().unwrap_or(0).max(0) as u64;
                        let modified = obj.last_modified().map(|d| d.secs().max(0) as u64);
                        entries.push(RemoteEntry {
                            name,
                            is_dir: false,
                            size,
                            modified,
                        });
                    }
                }

                if out.is_truncated().unwrap_or(false) {
                    token = out.next_continuation_token().map(|s| s.to_string());
                    if token.is_none() {
                        break;
                    }
                } else {
                    break;
                }
            }
            entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            Ok(entries)
        })
    }

    fn disconnect(&mut self) -> Result<(), String> {
        self.client = None;
        Ok(())
    }

    fn download_file(
        &mut self,
        remote: &str,
        local: &Path,
        resume_from: u64,
        on_progress: &mut ProgressFn,
    ) -> Result<(), String> {
        let client = self.client()?;
        let bucket = self.bucket.clone();
        let key = remote.trim_start_matches('/').to_string();
        let local = local.to_path_buf();
        self.rt.block_on(async move {
            let mut req = client.get_object().bucket(&bucket).key(&key);
            if resume_from > 0 {
                req = req.range(format!("bytes={resume_from}-"));
            }
            let mut resp = req
                .send()
                .await
                .map_err(|e| format!("No se pudo descargar: {}", s3_err(e)))?;
            use tokio::io::AsyncWriteExt;
            let mut file = if resume_from > 0 {
                tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(&local)
                    .await
                    .map_err(|e| format!("No se pudo abrir el archivo local: {e}"))?
            } else {
                tokio::fs::File::create(&local)
                    .await
                    .map_err(|e| format!("No se pudo crear el archivo local: {e}"))?
            };
            let mut total = resume_from;
            while let Some(chunk) = resp
                .body
                .next()
                .await
                .transpose()
                .map_err(|e| format!("Error leyendo: {e}"))?
            {
                file.write_all(&chunk)
                    .await
                    .map_err(|e| format!("Error escribiendo: {e}"))?;
                total += chunk.len() as u64;
                if !on_progress(total) {
                    return Err("Cancelado".to_string());
                }
            }
            file.flush().await.ok();
            Ok(())
        })
    }

    fn upload_file(
        &mut self,
        local: &Path,
        remote: &str,
        _resume_from: u64,
        on_progress: &mut ProgressFn,
    ) -> Result<(), String> {
        let client = self.client()?;
        let bucket = self.bucket.clone();
        let key = remote.trim_start_matches('/').to_string();
        let local = local.to_path_buf();
        let size = std::fs::metadata(&local).map(|m| m.len()).unwrap_or(0);

        // Archivos chicos: PUT directo. Grandes (>64MB): multipart (obligatorio >5GB).
        const THRESHOLD: u64 = 64 * 1024 * 1024;
        if size <= THRESHOLD {
            return self.rt.block_on(async move {
                let body = ByteStream::from_path(&local)
                    .await
                    .map_err(|e| format!("No se pudo leer el archivo local: {e}"))?;
                on_progress(0);
                client
                    .put_object()
                    .bucket(&bucket)
                    .key(&key)
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| format!("No se pudo subir: {}", s3_err(e)))?;
                on_progress(size);
                Ok(())
            });
        }

        // --- Subida multipart en PARALELO (streaming desde disco) ---
        // Parte >= 16MB (completa mas seguido -> progreso mas fluido) y calculada
        // para no pasar de ~9500 partes (limite S3 = 10000).
        let part_size: usize = {
            let min = 16 * 1024 * 1024u64;
            let needed = size / 9500 + 1;
            let ps = min.max(needed);
            (ps.div_ceil(1024 * 1024) * (1024 * 1024)) as usize
        };

        self.rt.block_on(async move {
            use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
            use std::sync::Arc;
            use tokio::sync::Semaphore;
            use tokio::task::JoinSet;

            // Concurrencia optima medida: 8 satura el ancho de banda de subida (mas
            // conexiones NO aceleran, solo agrandan las rafagas). ~8 * 16MB = 128MB pico.
            const CONCURRENCY: usize = 8;

            let mtime = std::fs::metadata(&local)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let num_parts = size.div_ceil(part_size as u64) as i32;
            let state_path = upload_state_path(&bucket, &key);

            async fn abort(client: &Client, bucket: &str, key: &str, upload_id: &str) {
                let _ = client
                    .abort_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .send()
                    .await;
            }

            // --- REANUDAR: si hay estado guardado que coincide, retomar el upload_id ---
            let mut upload_id = String::new();
            let mut done_parts: HashMap<i32, String> = HashMap::new();
            if let Some(sp) = &state_path {
                if let Some(st) = std::fs::read_to_string(sp)
                    .ok()
                    .and_then(|t| serde_json::from_str::<UploadState>(&t).ok())
                {
                    if st.bucket == bucket
                        && st.key == key
                        && st.file_size == size
                        && st.mtime_secs == mtime
                        && st.part_size == part_size as u64
                    {
                        // Confirmar con S3 que las partes siguen ahi.
                        if let Ok(listed) =
                            list_uploaded_parts(&client, &bucket, &key, &st.upload_id).await
                        {
                            upload_id = st.upload_id;
                            done_parts = listed;
                        }
                    }
                }
            }

            // Si no se pudo reanudar, iniciar una subida nueva y guardar el estado.
            if upload_id.is_empty() {
                let create = client
                    .create_multipart_upload()
                    .bucket(&bucket)
                    .key(&key)
                    .send()
                    .await
                    .map_err(|e| format!("No se pudo iniciar la subida: {}", s3_err(e)))?;
                upload_id = create
                    .upload_id()
                    .ok_or("El servidor no devolvio upload_id")?
                    .to_string();
                if let Some(sp) = &state_path {
                    save_upload_state(
                        sp,
                        &UploadState {
                            bucket: bucket.clone(),
                            key: key.clone(),
                            upload_id: upload_id.clone(),
                            file_size: size,
                            mtime_secs: mtime,
                            part_size: part_size as u64,
                        },
                    );
                }
            }

            let sem = Arc::new(Semaphore::new(CONCURRENCY));
            let mut tasks: JoinSet<(i32, Result<String, String>, u64)> = JoinSet::new();
            let mut parts: Vec<CompletedPart> = Vec::new();
            let mut completed: u64 = 0;
            let mut failed: Option<String> = None;
            let mut cancelled = false;

            // Contar/registrar las partes que ya estaban subidas (reanudacion).
            for (num, etag) in &done_parts {
                let off = (*num as u64 - 1) * part_size as u64;
                completed += (size - off).min(part_size as u64);
                parts.push(
                    CompletedPart::builder()
                        .e_tag(etag)
                        .part_number(*num)
                        .build(),
                );
            }
            on_progress(completed);

            fn handle(
                r: Result<(i32, Result<String, String>, u64), tokio::task::JoinError>,
                parts: &mut Vec<aws_sdk_s3::types::CompletedPart>,
                completed: &mut u64,
                failed: &mut Option<String>,
            ) {
                match r {
                    Ok((num, Ok(etag), len)) => {
                        parts.push(
                            aws_sdk_s3::types::CompletedPart::builder()
                                .e_tag(etag)
                                .part_number(num)
                                .build(),
                        );
                        *completed += len;
                    }
                    Ok((_, Err(e), _)) => {
                        if failed.is_none() {
                            *failed = Some(e);
                        }
                    }
                    Err(e) => {
                        if failed.is_none() {
                            *failed = Some(format!("tarea fallo: {e}"));
                        }
                    }
                }
            }

            // PRODUCTOR: un hilo lee por adelantado (prefetch) las siguientes partes
            // desde el disco y las deja en una cola, para que las subidas NUNCA esperen
            // al disco. La cola acotada limita la RAM (backpressure).
            let (tx, mut rx) = tokio::sync::mpsc::channel::<(i32, Vec<u8>)>(4);
            let reader_local = local.clone();
            let done_set: std::collections::HashSet<i32> = done_parts.keys().copied().collect();
            let ps = part_size;
            let reader = tokio::task::spawn_blocking(move || -> Result<(), String> {
                use std::io::{Read, Seek, SeekFrom};
                let mut f = std::fs::File::open(&reader_local)
                    .map_err(|e| format!("No se pudo abrir el archivo local: {e}"))?;
                for part_num in 1..=num_parts {
                    if done_set.contains(&part_num) {
                        continue; // ya subida (reanudacion)
                    }
                    let offset = (part_num as u64 - 1) * ps as u64;
                    let this_len = (size - offset).min(ps as u64) as usize;
                    if f.seek(SeekFrom::Start(offset)).is_err() {
                        return Err("Error posicionando el archivo".to_string());
                    }
                    let mut buf = vec![0u8; this_len];
                    let tr = std::time::Instant::now();
                    if f.read_exact(&mut buf).is_err() {
                        return Err("Error leyendo el archivo".to_string());
                    }
                    let rd_ms = tr.elapsed().as_millis();
                    if rd_ms > 120 {
                        eprintln!(
                            "[carrybox] LECTURA parte {part_num} ({} KB): {rd_ms}ms (disco lento)",
                            this_len / 1024
                        );
                    }
                    let ts = std::time::Instant::now();
                    if tx.blocking_send((part_num, buf)).is_err() {
                        break; // el consumidor se detuvo (cancel/error)
                    }
                    let send_ms = ts.elapsed().as_millis();
                    if send_ms > 120 {
                        eprintln!("[carrybox] COLA LLENA en parte {part_num}: el lector espero {send_ms}ms (las subidas van mas lento que el disco)");
                    }
                }
                Ok(())
            });

            // CONSUMIDOR: recibe partes ya leidas (listas) y las sube en paralelo.
            eprintln!(
                "[carrybox] multipart INICIO: {} partes de {} MB, concurrencia {}",
                num_parts,
                part_size / 1024 / 1024,
                CONCURRENCY
            );
            loop {
                let t_recv = std::time::Instant::now();
                let Some((part_num, buf)) = rx.recv().await else {
                    break;
                };
                let recv_ms = t_recv.elapsed().as_millis();
                let t_slot = std::time::Instant::now();
                let permit = sem.clone().acquire_owned().await.unwrap();
                let slot_ms = t_slot.elapsed().as_millis();
                if recv_ms > 120 || slot_ms > 120 {
                    eprintln!(
                        "[carrybox] parte {part_num}: espera_cola={recv_ms}ms espera_slot={slot_ms}ms (cola pendiente={})",
                        rx.len()
                    );
                }
                while let Some(r) = tasks.try_join_next() {
                    handle(r, &mut parts, &mut completed, &mut failed);
                }
                if failed.is_some() {
                    drop(permit);
                    break;
                }
                if !on_progress(completed) {
                    cancelled = true;
                    drop(permit);
                    break;
                }

                let c = client.clone();
                let b = bucket.clone();
                let k = key.clone();
                let uid = upload_id.clone();
                let n = part_num;
                let len = buf.len() as u64;
                tasks.spawn(async move {
                    let _permit = permit;
                    let tu = std::time::Instant::now();
                    // El cliente reintenta solo (retry_config); el body en RAM es reejecutable.
                    let res = c
                        .upload_part()
                        .bucket(&b)
                        .key(&k)
                        .upload_id(&uid)
                        .part_number(n)
                        .body(ByteStream::from(buf))
                        .send()
                        .await;
                    let up_ms = tu.elapsed().as_millis();
                    if up_ms > 60000 {
                        eprintln!("[carrybox] SUBIDA parte {n} MUY LENTA ({} KB): {up_ms}ms", len / 1024);
                    }
                    match res {
                        Ok(u) => (n, Ok(u.e_tag().unwrap_or_default().to_string()), len),
                        Err(e) => (
                            n,
                            Err(format!("Fallo al subir la parte {n}: {}", s3_err(e))),
                            len,
                        ),
                    }
                });
            }
            drop(rx); // liberar al productor si salimos temprano

            // Esperar a las partes en vuelo.
            while let Some(r) = tasks.join_next().await {
                handle(r, &mut parts, &mut completed, &mut failed);
                if failed.is_none() && !cancelled {
                    on_progress(completed);
                }
            }

            // Propagar un posible error de lectura del productor.
            match reader.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if failed.is_none() && !cancelled {
                        failed = Some(e);
                    }
                }
                Err(e) => {
                    if failed.is_none() && !cancelled {
                        failed = Some(format!("lector: {e}"));
                    }
                }
            }

            if cancelled {
                // Cancelacion del usuario: abortar y limpiar (no se reanuda).
                abort(&client, &bucket, &key, &upload_id).await;
                if let Some(sp) = &state_path {
                    let _ = std::fs::remove_file(sp);
                }
                return Err("Cancelado".to_string());
            }
            if let Some(e) = failed {
                // Fallo de red: NO abortamos ni borramos el estado -> se REANUDA luego.
                return Err(e);
            }

            // Las partes deben ir en orden ascendente para completar.
            parts.sort_by_key(|p| p.part_number().unwrap_or(0));
            let completed_mp = CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build();
            client
                .complete_multipart_upload()
                .bucket(&bucket)
                .key(&key)
                .upload_id(&upload_id)
                .multipart_upload(completed_mp)
                .send()
                .await
                .map_err(|e| format!("No se pudo completar la subida: {}", s3_err(e)))?;

            // VERIFICACION FINAL: el tamano en S3 debe coincidir con el local.
            let head = client
                .head_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| format!("No se pudo verificar la subida: {}", s3_err(e)))?;
            let remote_size = head.content_length().unwrap_or(-1);
            if remote_size != size as i64 {
                return Err(format!(
                    "Verificacion FALLIDA: en S3 hay {remote_size} bytes pero el original tiene {size}. La subida NO es confiable."
                ));
            }

            // Exito verificado -> borrar el estado de reanudacion.
            if let Some(sp) = &state_path {
                let _ = std::fs::remove_file(sp);
            }
            Ok(())
        })
    }

    fn mkdir(&mut self, remote: &str) -> Result<(), String> {
        let client = self.client()?;
        let bucket = self.bucket.clone();
        let key = norm_prefix(remote); // marcador "prefix/"
        self.rt.block_on(async move {
            client
                .put_object()
                .bucket(&bucket)
                .key(&key)
                .body(ByteStream::from_static(b""))
                .send()
                .await
                .map_err(|e| format!("No se pudo crear la carpeta: {}", s3_err(e)))?;
            Ok(())
        })
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        let client = self.client()?;
        let bucket = self.bucket.clone();
        let from_key = from.trim_start_matches('/').to_string();
        let to_key = to.trim_start_matches('/').to_string();
        self.rt.block_on(async move {
            client
                .copy_object()
                .bucket(&bucket)
                .copy_source(format!("{bucket}/{from_key}"))
                .key(&to_key)
                .send()
                .await
                .map_err(|e| format!("No se pudo renombrar: {}", s3_err(e)))?;
            client
                .delete_object()
                .bucket(&bucket)
                .key(&from_key)
                .send()
                .await
                .map_err(|e| format!("No se pudo borrar el original: {}", s3_err(e)))?;
            Ok(())
        })
    }

    fn delete_file(&mut self, remote: &str) -> Result<(), String> {
        let client = self.client()?;
        let bucket = self.bucket.clone();
        let key = remote.trim_start_matches('/').to_string();
        self.rt.block_on(async move {
            client
                .delete_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| format!("No se pudo eliminar: {}", s3_err(e)))?;
            Ok(())
        })
    }

    fn delete_dir(&mut self, remote: &str) -> Result<(), String> {
        // El marcador de carpeta; los hijos los borra la recursion de lib.rs.
        let client = self.client()?;
        let bucket = self.bucket.clone();
        let key = norm_prefix(remote);
        self.rt.block_on(async move {
            let _ = client.delete_object().bucket(&bucket).key(&key).send().await;
            Ok(())
        })
    }
}
