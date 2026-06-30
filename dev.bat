@echo off
chcp 65001 >nul
title CarryBox - Desarrollo
cd /d "%~dp0"

echo ============================================
echo   CarryBox  -  Modo desarrollo (hot reload)
echo ============================================
echo.

REM 1) Instalar dependencias de Node solo si faltan
if not exist "node_modules" (
  echo [1/3] Instalando dependencias de Node ^(primera vez^)...
  call npm install
  if errorlevel 1 goto :error
) else (
  echo [1/3] Dependencias de Node ya instaladas. OK
)

REM 2) Abrir la carpeta del proyecto en el Explorador
echo [2/3] Abriendo la carpeta del proyecto...
start "" explorer "%cd%"

REM 3) Compilar y lanzar la app (la primera vez tarda varios minutos)
echo [3/3] Compilando y abriendo CarryBox...
echo       (La ventana de la app se abrira sola. Deja esta consola abierta.)
echo.
call npm run tauri dev
if errorlevel 1 goto :error

goto :eof

:error
echo.
echo ********************************************
echo   Ocurrio un error. Revisa el mensaje arriba.
echo ********************************************
pause
