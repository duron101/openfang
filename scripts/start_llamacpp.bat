@echo off
setlocal enabledelayedexpansion

REM Start llama.cpp OpenAI-compatible server from bundled binaries.
REM Override model path:
REM   set LLAMACPP_MODEL=D:\models\your-model.gguf
REM   scripts\start_llamacpp.bat

set "ROOT=%~dp0.."
set "BIN=%ROOT%\public\llamaCPP\llama-server.exe"
set "HOST=127.0.0.1"
set "PORT=8080"

if not defined LLAMACPP_MODEL (
  echo [llamacpp] Set LLAMACPP_MODEL to your GGUF file, e.g. D:\models\qwen3-8b-q4_k_m.gguf
  exit /b 1
)

if not exist "%BIN%" (
  echo [llamacpp] Binary not found: %BIN%
  exit /b 1
)

if not exist "%LLAMACPP_MODEL%" (
  echo [llamacpp] Model not found: %LLAMACPP_MODEL%
  exit /b 1
)

echo [llamacpp] Starting %BIN%
echo [llamacpp] Model: %LLAMACPP_MODEL%
echo [llamacpp] URL:   http://%HOST%:%PORT%/v1

cd /d "%ROOT%\public\llamaCPP"
set GGML_VK_DISABLE_COOPMAT=1
"%BIN%" -m "%LLAMACPP_MODEL%" --host %HOST% --port %PORT% -ngl 99 --fit off %*
