@echo off
chcp 65001 >nul
setlocal

echo ===================================================================
echo             CLOUD UPLOADER - RELEASE BUILDER (ZIG)
echo ===================================================================
echo.

REM 1. Check zig
where zig >nul 2>nul
if errorlevel 1 goto ERR_NO_ZIG

REM 2. Check cargo-zigbuild
where cargo-zigbuild >nul 2>nul
if errorlevel 1 goto INSTALL_ZIGBUILD
goto CHECK_TARGET

:INSTALL_ZIGBUILD
echo [*] cargo-zigbuild is not installed. Installing...
cargo install cargo-zigbuild
if errorlevel 1 goto ERR_INSTALL_ZIGBUILD

:CHECK_TARGET
echo [*] Checking Rust target aarch64-unknown-linux-musl...
rustup target add aarch64-unknown-linux-musl >nul 2>nul

if not exist dist mkdir dist

set TARGET_MODE=%1
if "%TARGET_MODE%"=="" set TARGET_MODE=all

REM ---------------------------------------------------------------------
REM Build Windows x86_64 EXE
REM ---------------------------------------------------------------------
if "%TARGET_MODE%"=="arm64" goto BUILD_ARM64

echo.
echo ===================================================================
echo [1/2] Building Windows x86_64 (.exe)...
echo ===================================================================
cargo build --release
if errorlevel 1 goto ERR_BUILD_EXE

copy /Y "target\release\cloud-uploader.exe" "dist\cloud-uploader.exe" >nul
echo [OK] Windows binary ready: dist\cloud-uploader.exe

if "%TARGET_MODE%"=="exe" goto ALL_DONE

REM ---------------------------------------------------------------------
REM Build Linux ARM64 (aarch64-unknown-linux-musl via Zig)
REM ---------------------------------------------------------------------
:BUILD_ARM64
echo.
echo ===================================================================
echo [2/2] Building Linux ARM64 (aarch64-unknown-linux-musl via Zig)...
echo ===================================================================
cargo zigbuild --release --target aarch64-unknown-linux-musl
if errorlevel 1 goto ERR_BUILD_ARM64

copy /Y "target\aarch64-unknown-linux-musl\release\cloud-uploader" "dist\cloud-uploader-linux-arm64" >nul
echo [OK] Linux ARM64 binary ready: dist\cloud-uploader-linux-arm64

:ALL_DONE
echo.
echo ===================================================================
echo                      BUILD SUCCEEDED!
echo ===================================================================
echo Artifacts saved in dist/ folder:
echo.
if exist "dist\cloud-uploader.exe" (
    for %%F in ("dist\cloud-uploader.exe") do echo   [Windows x64]  dist\cloud-uploader.exe            [%%~zF bytes]
)
if exist "dist\cloud-uploader-linux-arm64" (
    for %%F in ("dist\cloud-uploader-linux-arm64") do echo   [Linux ARM64]  dist\cloud-uploader-linux-arm64   [%%~zF bytes]
)
echo ===================================================================
echo.
if "%NO_PAUSE%"=="" pause
exit /b 0

:ERR_NO_ZIG
echo [ERROR] zig was not found in PATH.
echo Please install Zig: winget install zig.zig
echo.
if "%NO_PAUSE%"=="" pause
exit /b 1

:ERR_INSTALL_ZIGBUILD
echo [ERROR] Failed to install cargo-zigbuild.
echo.
if "%NO_PAUSE%"=="" pause
exit /b 1

:ERR_BUILD_EXE
echo [ERROR] Windows EXE build failed!
echo.
if "%NO_PAUSE%"=="" pause
exit /b 1

:ERR_BUILD_ARM64
echo [ERROR] Linux ARM64 build failed!
echo.
if "%NO_PAUSE%"=="" pause
exit /b 1
