@echo off
call "D:\program files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
cargo check -p openfang-platform -p openfang-platform-arksim 2>&1
