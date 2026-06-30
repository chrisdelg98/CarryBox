import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

// ---- Tipos que reflejan el backend Rust ----
type RemoteEntry = {
  name: string;
  is_dir: boolean;
  size: number;
  modified: number | null;
};
type LocalEntry = { name: string; path: string; is_dir: boolean; size: number };
type LocalDir = { path: string; parent: string | null; entries: LocalEntry[] };

type LogKind = "status" | "command" | "response" | "error";
type LogItem = { kind: LogKind; text: string; time: string };

function fmtSize(bytes: number): string {
  if (bytes <= 0) return "";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let n = bytes;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
}

function fmtDate(epoch: number | null): string {
  if (!epoch) return "";
  const d = new Date(epoch * 1000);
  return d.toLocaleDateString() + " " + d.toLocaleTimeString().slice(0, 5);
}

function joinRemote(base: string, name: string): string {
  if (base.endsWith("/")) return base + name;
  return base + "/" + name;
}
function parentRemote(path: string): string {
  if (path === "/" || path === "") return "/";
  const trimmed = path.replace(/\/+$/, "");
  const idx = trimmed.lastIndexOf("/");
  return idx <= 0 ? "/" : trimmed.slice(0, idx);
}

// Etiqueta + color por tipo de linea (estilo FileZilla).
const LOG_STYLE: Record<LogKind, { label: string; cls: string }> = {
  status: { label: "Estado", cls: "text-slate-300" },
  command: { label: "Comando", cls: "text-sky-400" },
  response: { label: "Respuesta", cls: "text-emerald-400" },
  error: { label: "Error", cls: "text-rose-400" },
};

export default function App() {
  // --- Conexion ---
  type Protocol = "sftp" | "ftp" | "ftps";
  const [protocol, setProtocol] = useState<Protocol>("sftp");
  const [host, setHost] = useState("");
  const [port, setPort] = useState(22);
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [passive, setPassive] = useState(true); // modo pasivo FTP (avanzado)
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [connected, setConnected] = useState(false);
  const [busy, setBusy] = useState(false);

  // Al cambiar de protocolo, ajustar el puerto por defecto (22 SFTP / 21 FTP).
  function changeProtocol(p: Protocol) {
    setProtocol(p);
    setPort(p === "sftp" ? 22 : 21);
  }

  // --- Paneles ---
  const [localDir, setLocalDir] = useState<LocalDir | null>(null);
  const [remotePath, setRemotePath] = useState("/");
  const [remoteEntries, setRemoteEntries] = useState<RemoteEntry[]>([]);

  // --- Log ---
  const [log, setLog] = useState<LogItem[]>([]);
  const logRef = useRef<HTMLDivElement>(null);
  const addLog = (kind: LogKind, text: string) =>
    setLog((l) => [
      ...l.slice(-499),
      { kind, text, time: new Date().toLocaleTimeString() },
    ]);

  useEffect(() => {
    logRef.current?.scrollTo(0, logRef.current.scrollHeight);
  }, [log]);

  // Escuchar el log en vivo que emite el backend (fases + comandos/respuestas FTP).
  useEffect(() => {
    const un = listen<{ kind: LogKind; text: string }>("ftp-log", (e) => {
      addLog(e.payload.kind, e.payload.text);
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // Cargar carpeta local (home) al iniciar.
  useEffect(() => {
    loadLocal(null);
  }, []);

  async function loadLocal(path: string | null) {
    try {
      const dir = await invoke<LocalDir>("list_local", { path });
      setLocalDir(dir);
    } catch (e) {
      addLog("error", `Local: ${e}`);
    }
  }

  async function connect() {
    if (!host) {
      addLog("error", "Falta el host.");
      return;
    }
    setBusy(true);
    try {
      const cwd = await invoke<string>("remote_connect", {
        config: { protocol, host, port, username, password, passive },
      });
      setConnected(true);
      await openRemote(cwd || "/");
    } catch {
      // El backend ya emitio el detalle del error al log.
      setConnected(false);
    } finally {
      setBusy(false);
    }
  }

  function disconnect() {
    // UI optimista: liberamos la pantalla de inmediato y cerramos por detras,
    // asi el boton nunca se ve "colgado" aunque la red tarde.
    setConnected(false);
    setRemoteEntries([]);
    setRemotePath("/");
    setBusy(false);
    invoke("remote_disconnect").catch((e) => addLog("error", `${e}`));
  }

  async function openRemote(path: string) {
    setBusy(true);
    try {
      const entries = await invoke<RemoteEntry[]>("remote_list", { path });
      setRemoteEntries(entries);
      setRemotePath(path);
    } catch {
      // error ya emitido por el backend
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex h-full flex-col bg-slate-900 text-slate-200">
      {/* Barra superior: marca + conexion rapida */}
      <header className="border-b border-slate-700 bg-slate-800 px-4 py-2">
        <div className="mb-2 flex items-center gap-2">
          <span className="text-lg font-semibold text-sky-400">CarryBox</span>
          <span className="text-xs text-slate-400">· Descargar · FTP / FTPS (estilo FileZilla)</span>
        </div>
        <div className="flex flex-wrap items-end gap-2">
          <Field label="Protocolo">
            <select
              className="input w-40"
              value={protocol}
              onChange={(e) => changeProtocol(e.target.value as Protocol)}
              disabled={connected}
            >
              <option value="sftp">SFTP (SSH) · recomendado</option>
              <option value="ftp">FTP</option>
              <option value="ftps">FTPS (FTP cifrado)</option>
            </select>
          </Field>
          <Field label="Servidor (host)">
            <input
              className="input w-52"
              value={host}
              placeholder="midominio.com o IP"
              onChange={(e) => setHost(e.target.value)}
              disabled={connected}
            />
          </Field>
          <Field label="Usuario">
            <input
              className="input w-36"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              disabled={connected}
            />
          </Field>
          <Field label="Contrasena">
            <input
              type="password"
              className="input w-36"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              disabled={connected}
            />
          </Field>
          <Field label="Puerto">
            <input
              type="number"
              className="input w-20"
              value={port}
              onChange={(e) => setPort(Number(e.target.value) || 21)}
              disabled={connected}
            />
          </Field>
          {!connected ? (
            <button className="btn btn-primary" onClick={connect} disabled={busy}>
              {busy ? "Conectando..." : "Conectar"}
            </button>
          ) : (
            <button className="btn btn-danger" onClick={disconnect}>
              Desconectar
            </button>
          )}
          <button
            className="btn-mini self-end pb-1.5"
            onClick={() => setShowAdvanced((v) => !v)}
            title="Opciones avanzadas"
          >
            {showAdvanced ? "▾ Avanzado" : "▸ Avanzado"}
          </button>
        </div>

        {/* Opciones avanzadas */}
        {showAdvanced && (
          <div className="mt-2 flex flex-wrap items-center gap-4 border-t border-slate-700 pt-2 text-xs text-slate-300">
            {protocol === "sftp" ? (
              <span className="text-slate-500">
                SFTP usa una sola conexion (puerto 22); no necesita modo pasivo.
              </span>
            ) : (
              <label className="flex items-center gap-1.5">
                <input
                  type="checkbox"
                  checked={passive}
                  onChange={(e) => setPassive(e.target.checked)}
                  disabled={connected}
                />
                Modo pasivo (recomendado tras NAT/router)
              </label>
            )}
          </div>
        )}
      </header>

      {/* Panel doble */}
      <div className="flex min-h-0 flex-1">
        {/* LOCAL */}
        <Pane title="Local (tu PC)">
          <PathBar
            path={localDir?.path ?? ""}
            onUp={() => localDir?.parent && loadLocal(localDir.parent)}
            canUp={!!localDir?.parent}
          />
          <FileTable
            rows={
              localDir?.entries.map((e) => ({
                key: e.path,
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
                modified: null,
                onOpen: () => e.is_dir && loadLocal(e.path),
              })) ?? []
            }
          />
        </Pane>

        {/* REMOTO */}
        <Pane title="Remoto (servidor)">
          <PathBar
            path={connected ? remotePath : "—"}
            onUp={() => openRemote(parentRemote(remotePath))}
            canUp={connected && remotePath !== "/"}
          />
          {connected ? (
            <FileTable
              rows={remoteEntries.map((e) => ({
                key: e.name,
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
                modified: e.modified,
                onOpen: () => e.is_dir && openRemote(joinRemote(remotePath, e.name)),
              }))}
            />
          ) : (
            <div className="flex h-full items-center justify-center px-4 text-center text-sm text-slate-500">
              No conectado. Llena los datos y pulsa “Conectar”.
            </div>
          )}
        </Pane>
      </div>

      {/* Log estilo FileZilla */}
      <div className="flex h-40 flex-col border-t border-slate-700">
        <div className="flex items-center justify-between bg-slate-800 px-3 py-1 text-xs text-slate-400">
          <span className="font-semibold uppercase tracking-wide">Registro</span>
          <button className="btn-mini" onClick={() => setLog([])}>
            Limpiar
          </button>
        </div>
        <div
          ref={logRef}
          className="min-h-0 flex-1 overflow-auto bg-black/50 px-3 py-1.5 font-mono text-xs leading-5"
        >
          {log.length === 0 ? (
            <div className="text-slate-600">Registro de eventos…</div>
          ) : (
            log.map((l, i) => (
              <div key={i} className="flex gap-2">
                <span className="shrink-0 text-slate-600">{l.time}</span>
                <span className={`shrink-0 w-20 ${LOG_STYLE[l.kind].cls}`}>
                  {LOG_STYLE[l.kind].label}:
                </span>
                <span className={LOG_STYLE[l.kind].cls}>{l.text}</span>
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  );
}

// ---- Componentes auxiliares ----

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="flex flex-col gap-1 text-xs text-slate-400">
      {label}
      {children}
    </label>
  );
}

function Pane({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="flex min-w-0 flex-1 flex-col border-r border-slate-700 last:border-r-0">
      <div className="bg-slate-800 px-3 py-1.5 text-xs font-semibold uppercase tracking-wide text-slate-400">
        {title}
      </div>
      {children}
    </section>
  );
}

function PathBar({
  path,
  onUp,
  canUp,
}: {
  path: string;
  onUp: () => void;
  canUp: boolean;
}) {
  return (
    <div className="flex items-center gap-2 border-b border-slate-700 bg-slate-800/60 px-2 py-1">
      <button className="btn-mini" onClick={onUp} disabled={!canUp} title="Subir un nivel">
        ↑
      </button>
      <span className="truncate font-mono text-xs text-slate-300" title={path}>
        {path}
      </span>
    </div>
  );
}

type Row = {
  key: string;
  name: string;
  is_dir: boolean;
  size: number;
  modified: number | null;
  onOpen: () => void;
};

function FileTable({ rows }: { rows: Row[] }) {
  return (
    <div className="min-h-0 flex-1 overflow-auto">
      <table className="w-full text-sm">
        <thead className="sticky top-0 bg-slate-800 text-left text-xs text-slate-400">
          <tr>
            <th className="px-3 py-1 font-medium">Nombre</th>
            <th className="px-3 py-1 font-medium">Tamano</th>
            <th className="px-3 py-1 font-medium">Modificado</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr
              key={r.key}
              onDoubleClick={r.onOpen}
              className="cursor-default border-b border-slate-800 hover:bg-slate-800/70"
            >
              <td className="px-3 py-1">
                <span className="mr-1.5">{r.is_dir ? "📁" : "📄"}</span>
                {r.name}
              </td>
              <td className="px-3 py-1 text-slate-400">{r.is_dir ? "" : fmtSize(r.size)}</td>
              <td className="px-3 py-1 text-slate-400">{fmtDate(r.modified)}</td>
            </tr>
          ))}
          {rows.length === 0 && (
            <tr>
              <td colSpan={3} className="px-3 py-3 text-xs text-slate-600">
                (carpeta vacia)
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </div>
  );
}
