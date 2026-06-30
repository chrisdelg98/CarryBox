# CarryBox

<p align="center">
	Desktop file transfer app for FTP, FTPS and SFTP.
</p>

<p align="center">
	<img src="https://img.shields.io/badge/Open%20Source-Yes-22c55e?style=for-the-badge" alt="Open Source" />
	<img src="https://img.shields.io/badge/Desktop-Tauri%202-24C8DB?style=for-the-badge&logo=tauri&logoColor=white" alt="Tauri" />
	<img src="https://img.shields.io/badge/Frontend-React%2019-61DAFB?style=for-the-badge&logo=react&logoColor=0b1020" alt="React" />
	<img src="https://img.shields.io/badge/Backend-Rust-000000?style=for-the-badge&logo=rust&logoColor=white" alt="Rust" />
</p>

<p align="center">
	<img src="https://img.shields.io/badge/TypeScript-5.8-3178C6?style=flat-square&logo=typescript&logoColor=white" alt="TypeScript" />
	<img src="https://img.shields.io/badge/Vite-7-646CFF?style=flat-square&logo=vite&logoColor=white" alt="Vite" />
	<img src="https://img.shields.io/badge/TailwindCSS-4-06B6D4?style=flat-square&logo=tailwindcss&logoColor=white" alt="TailwindCSS" />
	<img src="https://img.shields.io/badge/Protocol-FTP-0ea5e9?style=flat-square" alt="FTP" />
	<img src="https://img.shields.io/badge/Protocol-FTPS-0284c7?style=flat-square" alt="FTPS" />
	<img src="https://img.shields.io/badge/Protocol-SFTP-0f766e?style=flat-square" alt="SFTP" />
</p>

---

## English

CarryBox is an open-source desktop file transfer manager focused on a clean workflow for FTP, FTPS, and SFTP.

It is designed to feel familiar to users coming from classic file managers, while using a modern stack (Tauri + React + Rust) for speed, security, and low resource usage.

### Main Features

- Connect to FTP, FTPS, and SFTP servers.
- Two-panel file manager (local and remote).
- Upload and download files/folders with progress tracking.
- Transfer cancel support.
- Transfer logs (status, commands, responses, errors).
- Basic file operations: create folder, rename, delete.
- Conflict handling options on upload (replace, skip, newer, keep both).

---

## Espanol

CarryBox es una aplicacion de escritorio open source para transferir archivos por FTP, FTPS y SFTP con una experiencia clara y rapida.

Esta pensada para usuarios que prefieren una interfaz tipo gestor de archivos tradicional, pero con tecnologia moderna (Tauri + React + Rust).

### Funcionalidades principales

- Conexion a servidores FTP, FTPS y SFTP.
- Gestor de dos paneles (local y remoto).
- Subida y descarga de archivos/carpetas con progreso.
- Soporte para cancelar transferencias.
- Registro de eventos (estado, comandos, respuestas, errores).
- Operaciones basicas: crear carpeta, renombrar, eliminar.
- Manejo de conflictos al subir (reemplazar, omitir, solo mas nuevos, conservar ambos).

---

## Tech Stack

- Tauri 2
- React 19
- TypeScript
- Rust
- Vite
- Tailwind CSS
- suppaftp (FTP/FTPS)
- russh + russh-sftp (SFTP)

---

## Getting Started

### Requirements

- Node.js 20+
- npm
- Rust (stable)

### Development

```bash
npm install
npm run tauri dev
```

### Build

```bash
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
```

---

## Open Source and Credits

This project is open source.

You can use, modify, and adapt CarryBox for personal or commercial work, as long as you give clear credit to the original project and author.

Suggested credit line:

```text
Based on CarryBox by Christian Arevalo
https://github.com/chrisdelg98/CarryBox
```

---

## Uso y Creditos (ES)

Este proyecto es open source.

Puedes usarlo, modificarlo y adaptarlo para proyectos personales o comerciales, siempre que des credito claro al proyecto original y al autor.

Linea sugerida de credito:

```text
Basado en CarryBox por Christian Arevalo
https://github.com/chrisdelg98/CarryBox
```

---

## Can I Use This Project?

Yes. You can use CarryBox with attribution.

## Lo puedo usar?

Si. Puedes usar CarryBox dando creditos.
