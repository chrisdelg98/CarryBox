//! Modo Descargar — capa de proveedores remotos.
//!
//! Toda fuente remota (FTP hoy; SFTP/rsync despues) vive detras del trait
//! `RemoteSource`. Asi la UI y los comandos Tauri no saben de protocolos: solo
//! piden conectar / listar / descargar. Anadir un protocolo = otra impl del
//! trait, sin tocar el resto (mismo principio que `SecretStore` para multi-SO).

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Callback de progreso: recibe el total de bytes transferidos hasta ahora.
pub type ProgressFn<'a> = dyn FnMut(u64) + 'a;

fn default_true() -> bool {
    true
}

/// Datos de conexion que llegan desde la UI.
#[derive(Debug, Clone, Deserialize)]
pub struct ConnConfig {
    /// "sftp" | "ftp" | "ftps"
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// Modo pasivo (solo FTP/FTPS). Por defecto activado.
    #[serde(default = "default_true")]
    pub passive: bool,
}

/// Un elemento dentro de una carpeta remota.
#[derive(Debug, Clone, Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Epoch en segundos; la UI lo formatea. None si el server no lo da.
    pub modified: Option<u64>,
}

/// Contrato comun de cualquier proveedor remoto del modo Descargar.
/// Las operaciones trabajan a nivel de UN archivo / UNA carpeta; la recursion
/// (bajar/subir carpetas enteras) la orquesta lib.rs usando list_dir + estas.
pub trait RemoteSource: Send {
    fn connect(&mut self, cfg: &ConnConfig) -> Result<(), String>;
    /// Directorio de trabajo actual tras conectar.
    fn pwd(&mut self) -> Result<String, String>;
    fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>, String>;
    fn disconnect(&mut self) -> Result<(), String>;

    /// Descarga un archivo remoto a disco (streaming, sin copia completa en RAM).
    fn download_file(
        &mut self,
        remote: &str,
        local: &Path,
        on_progress: &mut ProgressFn,
    ) -> Result<(), String>;
    /// Sube un archivo local al remoto (streaming).
    fn upload_file(
        &mut self,
        local: &Path,
        remote: &str,
        on_progress: &mut ProgressFn,
    ) -> Result<(), String>;
    fn mkdir(&mut self, remote: &str) -> Result<(), String>;
    fn rename(&mut self, from: &str, to: &str) -> Result<(), String>;
    fn delete_file(&mut self, remote: &str) -> Result<(), String>;
    /// Borra una carpeta (debe estar vacia; la recursion la hace lib.rs).
    fn delete_dir(&mut self, remote: &str) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// FtpSource — proveedor FTP (suppaftp, version sync).
// ---------------------------------------------------------------------------

use suppaftp::list::File as FtpFile;
use suppaftp::native_tls::TlsConnector;
use suppaftp::types::Mode;
use suppaftp::NativeTlsFtpStream;

// NativeTlsFtpStream sirve para FTP plano y para FTPS: se conecta en claro y, si
// hace falta, se "asciende" a TLS con into_secure() (FTPS explicito / AUTH TLS).
type Ftp = NativeTlsFtpStream;

#[derive(Default)]
pub struct FtpSource {
    stream: Option<Ftp>,
}

impl FtpSource {
    pub fn new() -> Self {
        Self { stream: None }
    }
}

impl RemoteSource for FtpSource {
    fn connect(&mut self, cfg: &ConnConfig) -> Result<(), String> {
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let mut ftp =
            Ftp::connect(&addr).map_err(|e| format!("No se pudo conectar a {addr}: {e}"))?;

        // FTPS explicito (AUTH TLS) si el protocolo es "ftps".
        if cfg.protocol == "ftps" {
            // Aceptamos certificados invalidos: muchos hosts usan IP o cert
            // autofirmado. Es comodo para el usuario final; revisarlo si algun
            // dia queremos verificacion estricta opcional.
            let connector = TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build()
                .map_err(|e| format!("No se pudo iniciar TLS: {e}"))?;
            // Tipo explicito: suppaftp espera su wrapper NativeTlsConnector.
            let tls: suppaftp::NativeTlsConnector = connector.into();
            ftp = ftp
                .into_secure(tls, &cfg.host)
                .map_err(|e| format!("Fallo el cifrado FTPS/TLS: {e}"))?;
        }

        ftp.login(&cfg.username, &cfg.password)
            .map_err(|e| format!("Login fallo (usuario/clave?): {e}"))?;
        if cfg.passive {
            // Passive: lo normal cuando el cliente esta detras de NAT/router.
            ftp.set_mode(Mode::Passive);
            // Si el server anuncia una IP equivocada en PASV, usar la del control.
            ftp.set_passive_nat_workaround(true);
        } else {
            ftp.set_mode(Mode::Active);
        }
        self.stream = Some(ftp);
        Ok(())
    }

    fn pwd(&mut self) -> Result<String, String> {
        let ftp = self.stream.as_mut().ok_or("No conectado")?;
        ftp.pwd().map_err(|e| e.to_string())
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let ftp = self.stream.as_mut().ok_or("No conectado")?;
        let target = if path.is_empty() { None } else { Some(path) };
        let lines = ftp
            .list(target)
            .map_err(|e| format!("No se pudo listar la carpeta: {e}"))?;

        let mut entries = Vec::new();
        for line in lines {
            // Las lineas tipo "total 8" u otras no-archivo simplemente no parsean.
            if let Ok(f) = line.parse::<FtpFile>() {
                let name = f.name().to_string();
                if name == "." || name == ".." {
                    continue;
                }
                let modified = f
                    .modified()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_secs());
                entries.push(RemoteEntry {
                    name,
                    is_dir: f.is_directory(),
                    size: f.size() as u64,
                    modified,
                });
            }
        }
        // Carpetas primero, luego por nombre (como FileZilla).
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    fn disconnect(&mut self) -> Result<(), String> {
        // Cerrar de inmediato soltando el stream (eso cierra el socket TCP).
        // NO usamos quit(): manda QUIT y espera la respuesta del server, y si el
        // control esta medio muerto (p.ej. tras un timeout de datos) se cuelga
        // sin limite. Para un boton "Desconectar" preferimos cierre instantaneo.
        self.stream = None;
        Ok(())
    }

    fn download_file(
        &mut self,
        remote: &str,
        local: &Path,
        on_progress: &mut ProgressFn,
    ) -> Result<(), String> {
        let ftp = self.stream.as_mut().ok_or("No conectado")?;
        let mut data = ftp
            .retr_as_stream(remote)
            .map_err(|e| format!("No se pudo abrir el archivo remoto: {e}"))?;
        let mut file =
            std::fs::File::create(local).map_err(|e| format!("No se pudo crear el archivo local: {e}"))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = data.read(&mut buf).map_err(|e| format!("Error leyendo: {e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| format!("Error escribiendo en disco: {e}"))?;
            total += n as u64;
            on_progress(total);
        }
        ftp.finalize_retr_stream(data)
            .map_err(|e| format!("Error al finalizar la descarga: {e}"))?;
        Ok(())
    }

    fn upload_file(
        &mut self,
        local: &Path,
        remote: &str,
        on_progress: &mut ProgressFn,
    ) -> Result<(), String> {
        let ftp = self.stream.as_mut().ok_or("No conectado")?;
        let mut file =
            std::fs::File::open(local).map_err(|e| format!("No se pudo abrir el archivo local: {e}"))?;
        let mut data = ftp
            .put_with_stream(remote)
            .map_err(|e| format!("No se pudo crear el archivo remoto: {e}"))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = file.read(&mut buf).map_err(|e| format!("Error leyendo local: {e}"))?;
            if n == 0 {
                break;
            }
            data.write_all(&buf[..n])
                .map_err(|e| format!("Error subiendo: {e}"))?;
            total += n as u64;
            on_progress(total);
        }
        ftp.finalize_put_stream(data)
            .map_err(|e| format!("Error al finalizar la subida: {e}"))?;
        Ok(())
    }

    fn mkdir(&mut self, remote: &str) -> Result<(), String> {
        let ftp = self.stream.as_mut().ok_or("No conectado")?;
        ftp.mkdir(remote).map_err(|e| e.to_string())
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        let ftp = self.stream.as_mut().ok_or("No conectado")?;
        ftp.rename(from, to).map_err(|e| e.to_string())
    }

    fn delete_file(&mut self, remote: &str) -> Result<(), String> {
        let ftp = self.stream.as_mut().ok_or("No conectado")?;
        ftp.rm(remote).map_err(|e| e.to_string())
    }

    fn delete_dir(&mut self, remote: &str) -> Result<(), String> {
        let ftp = self.stream.as_mut().ok_or("No conectado")?;
        ftp.rmdir(remote).map_err(|e| e.to_string())
    }
}

/// Carpetas primero, luego por nombre (como FileZilla).
fn sort_entries(entries: &mut [RemoteEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

// ---------------------------------------------------------------------------
// SftpSource — proveedor SSH/SFTP (russh + russh-sftp, Rust puro).
//
// SFTP usa UNA sola conexion (puerto 22), sin canal de datos separado: por eso
// NO sufre el bloqueo de puertos pasivos que rompe el FTP en muchos servers.
// russh es async; como el trait RemoteSource es sync (se ejecuta dentro de
// spawn_blocking), cada operacion usa un runtime tokio propio con block_on.
// ---------------------------------------------------------------------------

use russh::client;
use russh::keys::ssh_key::PublicKey;
use russh_sftp::client::SftpSession;
use tokio::runtime::Runtime;

/// Handler SSH minimo: aceptamos cualquier host key (mejorable luego con
/// known_hosts / confirmacion del usuario).
struct SshHandler;

impl client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _key: &PublicKey) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct SftpSource {
    rt: Runtime,
    sftp: Option<SftpSession>,
    // Mantiene viva la sesion SSH mientras dure la conexion.
    _session: Option<client::Handle<SshHandler>>,
}

impl SftpSource {
    pub fn new() -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| format!("No se pudo crear el runtime async: {e}"))?;
        Ok(Self {
            rt,
            sftp: None,
            _session: None,
        })
    }
}

impl RemoteSource for SftpSource {
    fn connect(&mut self, cfg: &ConnConfig) -> Result<(), String> {
        let host = cfg.host.clone();
        let port = cfg.port;
        let user = cfg.username.clone();
        let pass = cfg.password.clone();

        let (session, sftp) = self.rt.block_on(async move {
            let config = std::sync::Arc::new(client::Config::default());
            let mut session = client::connect(config, (host.as_str(), port), SshHandler)
                .await
                .map_err(|e| format!("No se pudo conectar por SSH a {host}:{port}: {e}"))?;

            let auth = session
                .authenticate_password(&user, &pass)
                .await
                .map_err(|e| format!("Error de autenticacion SSH: {e}"))?;
            if !auth.success() {
                return Err("Autenticacion SSH fallida (usuario/clave?)".to_string());
            }

            let channel = session
                .channel_open_session()
                .await
                .map_err(|e| format!("No se pudo abrir el canal SSH: {e}"))?;
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|e| format!("El server no acepto el subsistema SFTP: {e}"))?;
            let sftp = SftpSession::new(channel.into_stream())
                .await
                .map_err(|e| format!("No se pudo iniciar SFTP: {e}"))?;

            Ok::<_, String>((session, sftp))
        })?;

        self._session = Some(session);
        self.sftp = Some(sftp);
        Ok(())
    }

    fn pwd(&mut self) -> Result<String, String> {
        let sftp = self.sftp.as_ref().ok_or("No conectado")?;
        self.rt.block_on(async {
            sftp.canonicalize(".")
                .await
                .map_err(|e| format!("No se pudo obtener la carpeta inicial: {e}"))
        })
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>, String> {
        let sftp = self.sftp.as_ref().ok_or("No conectado")?;
        let target = if path.is_empty() { "." } else { path };

        let mut entries = self.rt.block_on(async {
            let rd = sftp
                .read_dir(target)
                .await
                .map_err(|e| format!("No se pudo listar la carpeta: {e}"))?;
            let mut out = Vec::new();
            for entry in rd {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let meta = entry.metadata();
                out.push(RemoteEntry {
                    name,
                    is_dir: meta.is_dir(),
                    size: meta.size.unwrap_or(0),
                    modified: meta.mtime.map(|m| m as u64),
                });
            }
            Ok::<_, String>(out)
        })?;

        sort_entries(&mut entries);
        Ok(entries)
    }

    fn disconnect(&mut self) -> Result<(), String> {
        // Soltar la sesion cierra el socket de inmediato.
        self.sftp = None;
        self._session = None;
        Ok(())
    }

    fn download_file(
        &mut self,
        remote: &str,
        local: &Path,
        on_progress: &mut ProgressFn,
    ) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("No conectado")?;
        let remote = remote.to_string();
        let local = local.to_path_buf();
        self.rt.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut rfile = sftp
                .open(&remote)
                .await
                .map_err(|e| format!("No se pudo abrir el archivo remoto: {e}"))?;
            let mut lfile = tokio::fs::File::create(&local)
                .await
                .map_err(|e| format!("No se pudo crear el archivo local: {e}"))?;
            let mut buf = vec![0u8; 64 * 1024];
            let mut total: u64 = 0;
            loop {
                let n = rfile.read(&mut buf).await.map_err(|e| format!("Error leyendo: {e}"))?;
                if n == 0 {
                    break;
                }
                lfile
                    .write_all(&buf[..n])
                    .await
                    .map_err(|e| format!("Error escribiendo en disco: {e}"))?;
                total += n as u64;
                on_progress(total);
            }
            lfile.flush().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(())
        })
    }

    fn upload_file(
        &mut self,
        local: &Path,
        remote: &str,
        on_progress: &mut ProgressFn,
    ) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("No conectado")?;
        let remote = remote.to_string();
        let local = local.to_path_buf();
        self.rt.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut lfile = tokio::fs::File::open(&local)
                .await
                .map_err(|e| format!("No se pudo abrir el archivo local: {e}"))?;
            let mut rfile = sftp
                .create(&remote)
                .await
                .map_err(|e| format!("No se pudo crear el archivo remoto: {e}"))?;
            let mut buf = vec![0u8; 64 * 1024];
            let mut total: u64 = 0;
            loop {
                let n = lfile.read(&mut buf).await.map_err(|e| format!("Error leyendo local: {e}"))?;
                if n == 0 {
                    break;
                }
                rfile
                    .write_all(&buf[..n])
                    .await
                    .map_err(|e| format!("Error subiendo: {e}"))?;
                total += n as u64;
                on_progress(total);
            }
            rfile.flush().await.map_err(|e| e.to_string())?;
            rfile.shutdown().await.map_err(|e| e.to_string())?;
            Ok::<_, String>(())
        })
    }

    fn mkdir(&mut self, remote: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("No conectado")?;
        let p = remote.to_string();
        self.rt
            .block_on(async { sftp.create_dir(&p).await.map_err(|e| e.to_string()) })
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("No conectado")?;
        let (f, t) = (from.to_string(), to.to_string());
        self.rt
            .block_on(async { sftp.rename(&f, &t).await.map_err(|e| e.to_string()) })
    }

    fn delete_file(&mut self, remote: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("No conectado")?;
        let p = remote.to_string();
        self.rt
            .block_on(async { sftp.remove_file(&p).await.map_err(|e| e.to_string()) })
    }

    fn delete_dir(&mut self, remote: &str) -> Result<(), String> {
        let sftp = self.sftp.as_ref().ok_or("No conectado")?;
        let p = remote.to_string();
        self.rt
            .block_on(async { sftp.remove_dir(&p).await.map_err(|e| e.to_string()) })
    }
}
