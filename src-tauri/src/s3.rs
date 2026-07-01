//! Modo Subir a S3 — proveedor S3-compatible (aws-sdk-s3).
//!
//! Implementa el mismo `RemoteSource` que FTP/SFTP, de modo que la vista dual y
//! todo el motor de transferencia (progreso, cancelar, reintentos) se reutilizan.
//! En S3 no hay carpetas reales: se emulan con prefijos y delimitador "/".

use crate::remote::{ConnConfig, ProgressFn, RemoteEntry, RemoteSource};
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use std::path::Path;
use tokio::runtime::Runtime;

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

/// Mensaje legible de un error de aws-sdk.
fn s3_err<E: std::fmt::Display>(e: E) -> String {
    format!("{e}")
}

impl RemoteSource for S3Source {
    fn connect(&mut self, cfg: &ConnConfig) -> Result<(), String> {
        if cfg.endpoint.is_empty() || cfg.bucket.is_empty() {
            return Err("Faltan el endpoint o el bucket.".to_string());
        }
        let endpoint = cfg.endpoint.clone();
        let access = cfg.access_key.clone();
        let secret = cfg.secret_key.clone();
        let region = if cfg.region.is_empty() {
            "us-east-1".to_string()
        } else {
            cfg.region.clone()
        };
        let bucket = cfg.bucket.clone();
        let path_style = cfg.path_style;

        let client = self.rt.block_on(async move {
            let creds = Credentials::new(access, secret, None, None, "carrybox");
            let conf = aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new(region))
                .endpoint_url(endpoint)
                .credentials_provider(creds)
                .force_path_style(path_style)
                .build();
            let client = Client::from_conf(conf);
            // Pre-flight: valida credenciales + acceso al bucket.
            client
                .head_bucket()
                .bucket(&bucket)
                .send()
                .await
                .map_err(|e| format!("No se pudo acceder al bucket: {}", s3_err(e)))?;
            Ok::<_, String>(client)
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
        self.rt.block_on(async move {
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
