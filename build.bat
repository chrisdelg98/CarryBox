@echo off
chcp 65001 >nul
title CarryBox - Compilar instalador
cd /d "%~dp0"

echo ============================================
echo   CarryBox  -  Compilar instalador final
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

REM 2) Compilar el instalador de produccion (.msi / .exe). Tarda varios minutos.
echo [2/3] Compilando CarryBox para produccion...
echo       (Esto puede tardar varios minutos la primera vez.)
call npm run tauri build
if errorlevel 1 goto :error

REM 3) Abrir la carpeta donde quedo el instalador
echo [3/3] Abriendo la carpeta del instalador...
set "BUNDLE=%cd%\src-tauri\target\release\bundle"
if exist "%BUNDLE%" (
  start "" explorer "%BUNDLE%"
) else (
  echo No se encontro la carpeta del instalador en:
  echo   %BUNDLE%
)

echo.
echo Listo. Busca el instalador en las subcarpetas msi\ o nsis\.
pause
goto :eof

:error
echo.
echo ********************************************
echo   Ocurrio un error. Revisa el mensaje arriba.
echo ********************************************
pause
