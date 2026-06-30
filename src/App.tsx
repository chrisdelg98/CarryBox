import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

// ---- Tipos que reflejan el backend Rust ----
type RemoteEntry = { name: string; is_dir: boolean; size: number; modified: number | null };
type LocalEntry = { name: string; path: string; is_dir: boolean; size: number };
type LocalDir = { path: string; parent: string | null; entries: LocalEntry[] };

type Side = "local" | "remote";
type ItemRow = {
  name: string;
  path: string; // ruta completa (remota o local)
  is_dir: boolean;
  size: number;
  modified: number | null;
};

type LogKind = "status" | "command" | "response" | "error";
type LogItem = { kind: LogKind; text: string; time: string };

type Xfer = {
  kind: "download" | "upload";
  name: string;
  transferred: number;
  total: number;
  speed: number; // bytes/s
};

// ---- Formateadores ----
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
function fmtSpeed(bps: number): string {
  if (bps <= 0) return "";
  return fmtSize(bps) + "/s";
}
function fmtEta(sec: number): string {
  if (!isFinite(sec) || sec <= 0) return "";
  const s = Math.round(sec);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const r = s % 60;
  if (m < 60) return `${m}m ${r}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}
function joinRemote(base: string, name: string): string {
  return base.endsWith("/") ? base + name : base + "/" + name;
}
function parentRemote(path: string): string {
  if (path === "/" || path === "") return "/";
  const trimmed = path.replace(/\/+$/, "");
  const idx = trimmed.lastIndexOf("/");
  return idx <= 0 ? "/" : trimmed.slice(0, idx);
}

const LOG_STYLE: Record<LogKind, { label: string; cls: string }> = {
  status: { label: "Estado", cls: "text-slate-300" },
  command: { label: "Comando", cls: "text-sky-400" },
  response: { label: "Respuesta", cls: "text-emerald-400" },
  error: { label: "Error", cls: "text-rose-400" },
};

// Datos arrastrados entre paneles (fuera del estado para que onDrop los lea).
let dragData: { side: Side; items: ItemRow[] } | null = null;

// --- Preferencias persistentes (no incluye contrasena, por seguridad) ---
const PREFS_KEY = "carrybox.prefs";
function loadPrefs(): Record<string, unknown> {
  try {
    return JSON.parse(localStorage.getItem(PREFS_KEY) || "{}");
  } catch {
    return {};
  }
}
function savePrefs(patch: Record<string, unknown>) {
  try {
    const cur = loadPrefs();
    localStorage.setItem(PREFS_KEY, JSON.stringify({ ...cur, ...patch }));
  } catch {
    /* ignore */
  }
}

export default function App() {
  type Protocol = "sftp" | "ftp" | "ftps";
  const prefs0 = useRef(loadPrefs()).current;
  const [protocol, setProtocol] = useState<Protocol>((prefs0.protocol as Protocol) ?? "sftp");
  const [host, setHost] = useState<string>((prefs0.host as string) ?? "");
  const [port, setPort] = useState<number>((prefs0.port as number) ?? 22);
  const [username, setUsername] = useState<string>((prefs0.username as string) ?? "");
  const [password, setPassword] = useState("");
  const [passive, setPassive] = useState<boolean>((prefs0.passive as boolean) ?? true);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [connected, setConnected] = useState(false);
  const [busy, setBusy] = useState(false);

  const [localDir, setLocalDir] = useState<LocalDir | null>(null);
  const [remotePath, setRemotePath] = useState("/");
  const [remoteEntries, setRemoteEntries] = useState<RemoteEntry[]>([]);

  const [log, setLog] = useState<LogItem[]>([]);
  const logRef = useRef<HTMLDivElement>(null);
  const addLog = (kind: LogKind, text: string) =>
    setLog((l) => [...l.slice(-499), { kind, text, time: new Date().toLocaleTimeString() }]);

  // --- Menu contextual ---
  const [menu, setMenu] = useState<{ x: number; y: number; side: Side; row: ItemRow | null } | null>(
    null,
  );
  // --- Modal (prompt / confirm) ---
  type Modal = { type: "prompt" | "confirm"; title: string; value: string };
  const [modal, setModal] = useState<Modal | null>(null);
  const modalResolve = useRef<((v: string | boolean | null) => void) | null>(null);
  function askText(title: string, def = ""): Promise<string | null> {
    return new Promise((res) => {
      modalResolve.current = res as (v: string | boolean | null) => void;
      setModal({ type: "prompt", title, value: def });
    });
  }
  function askConfirm(title: string): Promise<boolean> {
    return new Promise((res) => {
      modalResolve.current = res as (v: string | boolean | null) => void;
      setModal({ type: "confirm", title, value: "" });
    });
  }
  function closeModal(result: string | boolean | null) {
    setModal(null);
    modalResolve.current?.(result);
    modalResolve.current = null;
  }

  // --- Transferencia en curso ---
  const [xfer, setXfer] = useState<Xfer | null>(null);
  const xferRef = useRef<{ t: number; bytes: number }>({ t: 0, bytes: 0 });

  useEffect(() => {
    logRef.current?.scrollTo(0, logRef.current.scrollHeight);
  }, [log]);

  // Log en vivo del backend.
  useEffect(() => {
    const un = listen<{ kind: LogKind; text: string }>("ftp-log", (e) => {
      addLog(e.payload.kind, e.payload.text);
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // Progreso de transferencias.
  useEffect(() => {
    const un = listen<{
      kind: "download" | "upload";
      state: "progress" | "done" | "error";
      name: string;
      transferred: number;
      total: number;
    }>("transfer", (e) => {
      const p = e.payload;
      if (p.state === "done") {
        setXfer(null);
        if (p.kind === "download") loadLocal(localDir?.path ?? null);
        else openRemote(remotePath);
        return;
      }
      if (p.state === "error") {
        setXfer(null);
        return;
      }
      // progress: calcular velocidad
      const now = performance.now();
      const prev = xferRef.current;
      let speed = 0;
      if (prev.t && now > prev.t) {
        speed = ((p.transferred - prev.bytes) * 1000) / (now - prev.t);
      }
      xferRef.current = { t: now, bytes: p.transferred };
      setXfer({ kind: p.kind, name: p.name, transferred: p.transferred, total: p.total, speed });
    });
    return () => {
      un.then((f) => f());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [localDir, remotePath]);

  // Cerrar menu al hacer clic en cualquier lado.
  useEffect(() => {
    const close = () => setMenu(null);
    window.addEventListener("click", close);
    return () => window.removeEventListener("click", close);
  }, []);

  useEffect(() => {
    loadLocal((prefs0.lastLocalDir as string) ?? null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Guardar preferencias de conexion (sin contrasena) cuando cambian.
  useEffect(() => {
    savePrefs({ protocol, host, port, username, passive });
  }, [protocol, host, port, username, passive]);

  function changeProtocol(p: Protocol) {
    setProtocol(p);
    setPort(p === "sftp" ? 22 : 21);
  }

  async function loadLocal(path: string | null) {
    try {
      const dir = await invoke<LocalDir>("list_local", { path });
      setLocalDir(dir);
      savePrefs({ lastLocalDir: dir.path }); // recordar la ultima carpeta
    } catch (e) {
      // Si la carpeta guardada ya no existe, caer a la carpeta de inicio.
      if (path) {
        loadLocal(null);
      } else {
        addLog("error", `Local: ${e}`);
      }
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
      setConnected(false);
    } finally {
      setBusy(false);
    }
  }

  function disconnect() {
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
      /* error ya emitido */
    } finally {
      setBusy(false);
    }
  }

  // ---- Filas normalizadas para cada panel ----
  const localRows: ItemRow[] =
    localDir?.entries.map((e) => ({
      name: e.name,
      path: e.path,
      is_dir: e.is_dir,
      size: e.size,
      modified: null,
    })) ?? [];
  const remoteRows: ItemRow[] = remoteEntries.map((e) => ({
    name: e.name,
    path: joinRemote(remotePath, e.name),
    is_dir: e.is_dir,
    size: e.size,
    modified: e.modified,
  }));

  // ---- Acciones de transferencia ----
  function toItems(rows: ItemRow[]) {
    return rows.map((r) => ({ name: r.name, path: r.path, is_dir: r.is_dir, size: r.size }));
  }
  async function downloadItems(rows: ItemRow[]) {
    if (!connected || !localDir) return;
    xferRef.current = { t: 0, bytes: 0 };
    await invoke("remote_download", { items: toItems(rows), localDir: localDir.path }).catch((e) =>
      addLog("error", `${e}`),
    );
  }
  async function uploadItems(rows: ItemRow[]) {
    if (!connected) {
      addLog("error", "Conéctate primero para subir.");
      return;
    }
    xferRef.current = { t: 0, bytes: 0 };
    await invoke("remote_upload", { items: toItems(rows), remoteDir: remotePath }).catch((e) =>
      addLog("error", `${e}`),
    );
  }

  // ---- Acciones de gestión ----
  async function remoteNewFolder() {
    const name = await askText("Nombre de la nueva carpeta (remota):", "nueva carpeta");
    if (!name) return;
    await invoke("remote_mkdir", { path: joinRemote(remotePath, name) }).catch((e) =>
      addLog("error", `${e}`),
    );
    openRemote(remotePath);
  }
  async function remoteRename(row: ItemRow) {
    const name = await askText("Nuevo nombre:", row.name);
    if (!name || name === row.name) return;
    await invoke("remote_rename", { from: row.path, to: joinRemote(remotePath, name) }).catch((e) =>
      addLog("error", `${e}`),
    );
    openRemote(remotePath);
  }
  async function remoteDelete(row: ItemRow) {
    const ok = await askConfirm(`¿Eliminar "${row.name}" del servidor?`);
    if (!ok) return;
    await invoke("remote_delete", { path: row.path, isDir: row.is_dir }).catch((e) =>
      addLog("error", `${e}`),
    );
    openRemote(remotePath);
  }
  async function localNewFolder() {
    if (!localDir) return;
    const name = await askText("Nombre de la nueva carpeta (local):", "nueva carpeta");
    if (!name) return;
    await invoke("local_mkdir", { parent: localDir.path, name }).catch((e) => addLog("error", `${e}`));
    loadLocal(localDir.path);
  }
  async function localRename(row: ItemRow) {
    const name = await askText("Nuevo nombre:", row.name);
    if (!name || name === row.name) return;
    await invoke("local_rename", { path: row.path, newName: name }).catch((e) =>
      addLog("error", `${e}`),
    );
    loadLocal(localDir?.path ?? null);
  }
  async function localDelete(row: ItemRow) {
    const ok = await askConfirm(`¿Eliminar "${row.name}" de tu PC?`);
    if (!ok) return;
    await invoke("local_delete", { path: row.path, isDir: row.is_dir }).catch((e) =>
      addLog("error", `${e}`),
    );
    loadLocal(localDir?.path ?? null);
  }

  // ---- Menu contextual: construir items segun lado/fila ----
  type MenuAct = {
    icon?: string;
    label?: string;
    onClick?: () => void;
    danger?: boolean;
    sep?: boolean;
  };
  function menuActions(): MenuAct[] {
    if (!menu) return [];
    const { side, row } = menu;
    const acts: MenuAct[] = [];
    if (side === "remote") {
      if (row) {
        acts.push({ icon: "download", label: "Descargar", onClick: () => downloadItems([row]) });
        acts.push({ icon: "pencil", label: "Renombrar", onClick: () => remoteRename(row) });
        acts.push({ icon: "trash", label: "Eliminar", onClick: () => remoteDelete(row), danger: true });
        acts.push({ sep: true });
      }
      acts.push({ icon: "folderPlus", label: "Nueva carpeta", onClick: () => remoteNewFolder() });
      acts.push({ icon: "refresh", label: "Actualizar", onClick: () => openRemote(remotePath) });
    } else {
      if (row) {
        acts.push({ icon: "upload", label: "Subir al servidor", onClick: () => uploadItems([row]) });
        acts.push({ icon: "pencil", label: "Renombrar", onClick: () => localRename(row) });
        acts.push({ icon: "trash", label: "Eliminar", onClick: () => localDelete(row), danger: true });
        acts.push({ sep: true });
      }
      acts.push({ icon: "folderPlus", label: "Nueva carpeta", onClick: () => localNewFolder() });
      acts.push({ icon: "refresh", label: "Actualizar", onClick: () => loadLocal(localDir?.path ?? null) });
    }
    return acts;
  }

  function onRowContextMenu(e: React.MouseEvent, side: Side, row: ItemRow | null) {
    e.preventDefault();
    e.stopPropagation();
    setMenu({ x: e.clientX, y: e.clientY, side, row });
  }
  function onDropTo(side: Side) {
    const d = dragData;
    dragData = null;
    if (!d) return;
    if (side === "local" && d.side === "remote") downloadItems(d.items);
    if (side === "remote" && d.side === "local") uploadItems(d.items);
  }

  const pct = xfer && xfer.total > 0 ? Math.min(100, (xfer.transferred / xfer.total) * 100) : 0;
  const eta = xfer && xfer.speed > 0 ? (xfer.total - xfer.transferred) / xfer.speed : 0;

  return (
    <div className="flex h-full flex-col bg-slate-900 text-slate-200">
      {/* Barra superior */}
      <header className="border-b border-slate-700 bg-slate-800 px-4 py-2">
        <div className="mb-2 flex items-center gap-2">
          <span className="text-lg font-semibold text-sky-400">CarryBox</span>
          <span className="text-xs text-slate-400">· Descargar · SFTP · FTP · FTPS</span>
        </div>
        <div className="flex items-end gap-2">
          <Field label="Protocolo" className="shrink-0">
            <select
              className="select w-60"
              value={protocol}
              onChange={(e) => changeProtocol(e.target.value as Protocol)}
              disabled={connected}
            >
              <option value="sftp">SFTP (SSH) · recomendado</option>
              <option value="ftp">FTP</option>
              <option value="ftps">FTPS (cifrado)</option>
            </select>
          </Field>
          <Field label="Servidor (host)" className="min-w-0 flex-2">
            <input
              className="input w-full"
              value={host}
              placeholder="midominio.com o IP"
              onChange={(e) => setHost(e.target.value)}
              disabled={connected}
            />
          </Field>
          <Field label="Usuario" className="min-w-0 flex-1">
            <input
              className="input w-full"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              disabled={connected}
            />
          </Field>
          <Field label="Contrasena" className="min-w-0 flex-1">
            <input
              type="password"
              className="input w-full"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              disabled={connected}
            />
          </Field>
          <Field label="Puerto" className="shrink-0">
            <input
              type="number"
              className="input w-20"
              value={port}
              onChange={(e) => setPort(Number(e.target.value) || 21)}
              disabled={connected}
            />
          </Field>
          <div className="flex shrink-0 items-end gap-2">
            {!connected ? (
              <button className="btn btn-primary w-36" onClick={connect} disabled={busy}>
                {busy ? "Conectando..." : "Conectar"}
              </button>
            ) : (
              <button className="btn btn-danger w-36" onClick={disconnect}>
                Desconectar
              </button>
            )}
            <button
              className="btn btn-secondary w-28"
              onClick={() => setShowAdvanced((v) => !v)}
              title="Opciones avanzadas"
            >
              {showAdvanced ? "▾ Avanzado" : "▸ Avanzado"}
            </button>
          </div>
        </div>

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
        <Pane
          title="Local (tu PC)"
          onDrop={() => onDropTo("local")}
          onContextMenu={(e) => onRowContextMenu(e, "local", null)}
        >
          <PathBar
            path={localDir?.path ?? ""}
            onUp={() => localDir?.parent && loadLocal(localDir.parent)}
            canUp={!!localDir?.parent}
          />
          <FileTable
            side="local"
            rows={localRows}
            onOpen={(r) => r.is_dir && loadLocal(r.path)}
            onContextMenu={onRowContextMenu}
          />
        </Pane>

        <Pane
          title="Remoto (servidor)"
          onDrop={() => onDropTo("remote")}
          onContextMenu={(e) => connected && onRowContextMenu(e, "remote", null)}
        >
          <PathBar
            path={connected ? remotePath : "—"}
            onUp={() => openRemote(parentRemote(remotePath))}
            canUp={connected && remotePath !== "/"}
          />
          {connected ? (
            <FileTable
              side="remote"
              rows={remoteRows}
              onOpen={(r) => r.is_dir && openRemote(r.path)}
              onContextMenu={onRowContextMenu}
            />
          ) : (
            <div className="flex flex-1 items-center justify-center px-4 text-center text-sm text-slate-500">
              No conectado. Llena los datos y pulsa “Conectar”.
            </div>
          )}
        </Pane>
      </div>

      {/* Barra de transferencia */}
      {xfer && (
        <div className="border-t border-slate-700 bg-slate-800 px-4 py-1.5">
          <div className="mb-1 flex items-center justify-between text-xs">
            <span className="truncate text-slate-200">
              {xfer.kind === "download" ? "⬇" : "⬆"} {xfer.name}
            </span>
            <span className="shrink-0 text-slate-400">
              {pct.toFixed(0)}% · {fmtSpeed(xfer.speed)}
              {eta ? ` · ETA ${fmtEta(eta)}` : ""}
            </span>
          </div>
          <div className="h-1.5 w-full overflow-hidden rounded bg-slate-700">
            <div className="h-full bg-sky-500 transition-all" style={{ width: `${pct}%` }} />
          </div>
        </div>
      )}

      {/* Log */}
      <div className="flex h-36 flex-col border-t border-slate-700">
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
                <span className={`w-20 shrink-0 ${LOG_STYLE[l.kind].cls}`}>
                  {LOG_STYLE[l.kind].label}:
                </span>
                <span className={LOG_STYLE[l.kind].cls}>{l.text}</span>
              </div>
            ))
          )}
        </div>
      </div>

      {/* Menu contextual */}
      {menu && (
        <ul
          className="fixed z-50 min-w-56 overflow-hidden rounded-lg border border-slate-700 bg-slate-800 py-1.5 text-sm shadow-2xl"
          style={{ left: menu.x, top: menu.y }}
          onClick={(e) => e.stopPropagation()}
        >
          {menuActions().map((a, i) =>
            a.sep ? (
              <li key={i} className="my-1.5 border-t border-slate-700/70" />
            ) : (
              <li
                key={i}
                className={`flex cursor-default items-center gap-3 px-3.5 py-2 ${
                  a.danger
                    ? "text-rose-400 hover:bg-rose-500/10"
                    : "text-slate-200 hover:bg-slate-700/70"
                }`}
                onClick={() => {
                  setMenu(null);
                  a.onClick?.();
                }}
              >
                <Icon name={a.icon ?? ""} className="h-4 w-4 shrink-0 opacity-80" />
                <span>{a.label}</span>
              </li>
            ),
          )}
        </ul>
      )}

      {/* Modal prompt/confirm */}
      {modal && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
          onClick={() => closeModal(modal.type === "confirm" ? false : null)}
        >
          <div
            className="w-80 rounded-lg border border-slate-600 bg-slate-800 p-4 shadow-2xl"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="mb-3 text-sm text-slate-200">{modal.title}</div>
            {modal.type === "prompt" && (
              <input
                autoFocus
                className="input mb-3 w-full"
                value={modal.value}
                onChange={(e) => setModal((m) => (m ? { ...m, value: e.target.value } : m))}
                onKeyDown={(e) => e.key === "Enter" && closeModal(modal.value)}
              />
            )}
            <div className="flex justify-end gap-2">
              <button
                className="btn btn-secondary"
                onClick={() => closeModal(modal.type === "confirm" ? false : null)}
              >
                Cancelar
              </button>
              <button
                className="btn btn-primary"
                onClick={() => closeModal(modal.type === "confirm" ? true : modal.value)}
              >
                Aceptar
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ---- Iconos (SVG uniformes, 16x16, mismo trazo) ----
function Icon({ name, className = "h-4 w-4" }: { name: string; className?: string }) {
  const p = {
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.8,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    className,
  };
  switch (name) {
    case "download":
      return (
        <svg {...p}>
          <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
          <path d="M7 10l5 5 5-5" />
          <path d="M12 15V3" />
        </svg>
      );
    case "upload":
      return (
        <svg {...p}>
          <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
          <path d="M17 8l-5-5-5 5" />
          <path d="M12 3v12" />
        </svg>
      );
    case "pencil":
      return (
        <svg {...p}>
          <path d="M12 20h9" />
          <path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4 12.5-12.5z" />
        </svg>
      );
    case "trash":
      return (
        <svg {...p}>
          <path d="M3 6h18" />
          <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
          <path d="M10 11v6M14 11v6" />
        </svg>
      );
    case "folderPlus":
      return (
        <svg {...p}>
          <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
          <path d="M12 11v6M9 14h6" />
        </svg>
      );
    case "refresh":
      return (
        <svg {...p}>
          <path d="M23 4v6h-6M1 20v-6h6" />
          <path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15" />
        </svg>
      );
    case "folder":
      return (
        <svg {...p}>
          <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
        </svg>
      );
    case "file":
      return (
        <svg {...p}>
          <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
          <path d="M14 2v6h6" />
        </svg>
      );
    default:
      return <svg {...p} />;
  }
}

// ---- Componentes auxiliares ----

function Field({
  label,
  children,
  className = "",
}: {
  label: string;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <label className={`flex flex-col gap-1 text-xs text-slate-400 ${className}`}>
      {label}
      {children}
    </label>
  );
}

function Pane({
  title,
  children,
  onDrop,
  onContextMenu,
}: {
  title: string;
  children: React.ReactNode;
  onDrop: () => void;
  onContextMenu: (e: React.MouseEvent) => void;
}) {
  const [over, setOver] = useState(false);
  return (
    <section
      className={`flex min-w-0 flex-1 flex-col border-r border-slate-700 last:border-r-0 ${
        over ? "bg-sky-500/5 ring-2 ring-inset ring-sky-500/40" : ""
      }`}
      onDragOver={(e) => {
        e.preventDefault();
        setOver(true);
      }}
      onDragLeave={() => setOver(false)}
      onDrop={(e) => {
        e.preventDefault();
        setOver(false);
        onDrop();
      }}
      onContextMenu={onContextMenu}
    >
      <div className="bg-slate-800 px-3 py-1.5 text-xs font-semibold uppercase tracking-wide text-slate-400">
        {title}
      </div>
      {children}
    </section>
  );
}

function PathBar({ path, onUp, canUp }: { path: string; onUp: () => void; canUp: boolean }) {
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

function summarize(rows: ItemRow[]): string {
  const dirs = rows.filter((r) => r.is_dir).length;
  const files = rows.length - dirs;
  return `${dirs} ${dirs === 1 ? "carpeta" : "carpetas"} · ${files} ${
    files === 1 ? "archivo" : "archivos"
  }`;
}

function FileTable({
  side,
  rows,
  onOpen,
  onContextMenu,
}: {
  side: Side;
  rows: ItemRow[];
  onOpen: (r: ItemRow) => void;
  onContextMenu: (e: React.MouseEvent, side: Side, row: ItemRow | null) => void;
}) {
  const [selected, setSelected] = useState<string | null>(null);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="min-h-0 flex-1 overflow-auto">
        <table className="w-full border-collapse text-sm">
          <thead className="sticky top-0 z-10 bg-slate-800 text-left text-xs text-slate-400">
            <tr className="border-b border-slate-700">
              <th className="px-3 py-1.5 font-medium">Nombre</th>
              <th className="px-3 py-1.5 text-right font-medium">Tamaño</th>
              <th className="px-3 py-1.5 font-medium">Modificado</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => {
              const isSel = selected === r.path;
              return (
                <tr
                  key={r.path}
                  draggable
                  onDragStart={() => {
                    dragData = { side, items: [r] };
                  }}
                  onClick={() => setSelected(r.path)}
                  onDoubleClick={() => onOpen(r)}
                  onContextMenu={(e) => {
                    setSelected(r.path);
                    onContextMenu(e, side, r);
                  }}
                  className={`cursor-default select-none ${
                    isSel ? "bg-sky-600/30" : "hover:bg-slate-800/60"
                  }`}
                >
                  <td className="max-w-0 truncate px-3 py-1.5" title={r.name}>
                    <span className="inline-flex items-center gap-2">
                      <Icon
                        name={r.is_dir ? "folder" : "file"}
                        className={`h-4 w-4 shrink-0 ${
                          r.is_dir ? "text-amber-400" : "text-slate-400"
                        }`}
                      />
                      <span className="truncate">{r.name}</span>
                    </span>
                  </td>
                  <td className="whitespace-nowrap px-3 py-1.5 text-right text-slate-400">
                    {r.is_dir ? "" : fmtSize(r.size)}
                  </td>
                  <td className="whitespace-nowrap px-3 py-1.5 text-slate-400">
                    {fmtDate(r.modified)}
                  </td>
                </tr>
              );
            })}
            {rows.length === 0 && (
              <tr>
                <td colSpan={3} className="px-3 py-3 text-xs text-slate-600">
                  (carpeta vacía)
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <div className="border-t border-slate-700/70 bg-slate-800/40 px-3 py-1 text-xs text-slate-500">
        {summarize(rows)}
      </div>
    </div>
  );
}
