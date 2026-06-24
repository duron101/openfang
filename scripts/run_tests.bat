@echo off
call "D:\program files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
cargo test -p openfang-types -p openfang-platform -p openfang-platform-arksim -p openfang-runtime -p openfang-kernel --lib 2>&1
