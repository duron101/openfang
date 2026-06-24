@echo off
call "D:\program files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
cd /d E:\dev\openfang
start "OpenFang" /B target\release\openfang.exe start
echo Daemon starting...
timeout /t 5 /nobreak >nul
curl -s http://127.0.0.1:4200/api/health
