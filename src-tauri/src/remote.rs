//! Modo Descargar — capa de proveedores remotos.
//!
//! Toda fuente remota (FTP hoy; SFTP/rsync despues) vive detras del trait
//! `RemoteSource`. Asi la UI y los comandos Tauri no saben de protocolos: solo
//! piden conectar / listar / descargar. Anadir un protocolo = otra impl del
//! trait, sin tocar el resto (mismo principio que `SecretStore` para multi-SO).

use serde::{Deserialize, Serialize};
use std::time::UNIX_EPOCH;

/// Datos de conexion que llegan desde la UI.
#[derive(Debug, Clone, Deserialize)]
pub struct ConnConfig {
    /// "ftp" por ahora; luego "sftp", "rsync"...
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// FTPS explicito (aun no implementado). Reservado para no romper la API.
    #[serde(default)]
    pub secure: bool,
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
pub trait RemoteSource: Send {
    fn connect(&mut self, cfg: &ConnConfig) -> Result<(), String>;
    /// Directorio de trabajo actual tras conectar.
    fn pwd(&mut self) -> Result<String, String>;
    fn list_dir(&mut self, path: &str) -> Result<Vec<RemoteEntry>, String>;
    fn disconnect(&mut self) -> Result<(), String>;
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

        // FTPS explicito (AUTH TLS) si el usuario lo pide.
        if cfg.secure {
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
        // Passive: imprescindible cuando el cliente esta detras de NAT/router.
        ftp.set_mode(Mode::Passive);
        // Si el server anuncia una IP equivocada en PASV, usar la del control.
        ftp.set_passive_nat_workaround(true);
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
}
