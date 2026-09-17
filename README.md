# Cloud Uploader (Rust Edition) 🦀

Автономная, быстрая и безопасная утилита на языке **Rust** для выгрузки бэкапов в облачные хранилища и автоматического зеркалирования релизов по ссылкам.

---

## ✨ Ключевые возможности

1. **Поддержка множества облачных провайдеров:**
   * 🇷🇺 **Яндекс.Диск** (REST API, OAuth2).
   * 🇷🇺 **Облако Mail.ru** (WebDAV `webdav.mail.ru`).
   * 🌐 **Google Drive** (Google Drive API v3).
   * 📦 **S3 / Cloudflare R2 / MinIO / Yandex Cloud Object Storage** (AWS SigV4).
   * ☁️ **Универсальный WebDAV** (Nextcloud, ownCloud, pCloud, Synology NAS).

2. **Режим 1: Выгрузка локальных файлов и папок (Бэкапы):**
   * Передайте путь к файлу или папке — утилита загрузит их в облако.
   * При передаче папки рекурсивно обходится все дерево файлов с автоматическим воссозданием каталогов на удаленном диске.
   * **Smart Skip:** файлы с одинаковым размером и подтвержденным MD5-хэшем повторно не перезаливаются.

3. **Режим 2: Зеркалирование ссылок и релизов GitHub (urls.txt):**
   * Если локальный путь не передан, утилита ищет `urls.txt`:
     1. Локально рядом с бинарником или в текущей папке (`./urls.txt`).
     2. В целевой папке облака (`{remote_dir}/urls.txt`).
     3. В корне облака (`/urls.txt`).
     4. Также можно передать конкретный путь через флаг `--urls-file <PATH>`.
   * Для ссылок `github.com/owner/repo` автоматически запрашивается последний релиз, скачиваются ассеты (`.apk`, `.exe`, `.zip`, `.tar.gz`) и удаляются старые версии этого же файла (безопасный поиск с защитой чужих файлов).
   * Поддержка `GITHUB_TOKEN` для расширения лимитов GitHub API до 5000 запросов/час.

4. **Отказоустойчивость:**
   * Настроены тайм-ауты соединения и передачи данных.
   * Встроенный механизм повторных попыток (retries) с экспоненциальной задержкой при временных сетевых сбоях.
   * Строгий Fail-Fast при отсутствии доступа к целевой папке.

---

## 🚀 Сборка

Требуется установленный Rust (1.75+):

### Быстрая сборка через скрипт (Windows + Linux ARM64 via Zig):

```cmd
build.bat          # Собрать и .exe, и Linux ARM64
build.bat exe      # Собрать только Windows .exe
build.bat arm64    # Собрать только Linux ARM64
```
Готовые бинарники автоматически складываются в папку `dist/`.

### Ручная сборка:
```bash
# Windows x86_64
cargo build --release

# Linux x86_64 (musl)
cargo build --release --target x86_64-unknown-linux-musl

# Linux ARM64 (aarch64 musl через zig)
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

---

## ⚙️ Настройка (.env)

Создайте файл `.env` в папке с программой (или скопируйте из `.env.example`):

```env
# Провайдер по умолчанию (yandex, mailru, google, webdav, s3)
CLOUD_PROVIDER=yandex
CLOUD_REMOTE_DIR=/Upload

# Яндекс Диск
YANDEX_TOKEN=y0__your_oauth_token_here

# Mail.ru Cloud (WebDAV)
MAILRU_USER=username@mail.ru
MAILRU_PASSWORD=app_password_here

# Google Drive
GOOGLE_DRIVE_TOKEN=ya29.your_token_here

# S3 / Cloudflare R2
S3_ENDPOINT=https://<account_id>.r2.cloudflarestorage.com
S3_BUCKET=my-backups
S3_ACCESS_KEY_ID=your_access_key
S3_SECRET_ACCESS_KEY=your_secret_key
S3_REGION=auto

# Универсальный WebDAV (Nextcloud и др.)
WEBDAV_URL=https://nextcloud.example.com/remote.php/dav/files/user/
WEBDAV_USER=nextcloud_user
WEBDAV_PASSWORD=nextcloud_password

# Опционально: токен для GitHub API
GITHUB_TOKEN=ghp_your_token_here
```

---

## 📖 Использование

### Справка:
```text
cloud-uploader [OPTIONS] [LOCAL_PATH]

Arguments:
  [LOCAL_PATH]  Local file or directory to upload (if omitted, runs remote URLs mirror mode)

Options:
  -p, --provider <PROVIDER>  Cloud provider [env: CLOUD_PROVIDER=] [default: yandex] [possible values: yandex, mailru, google, webdav, s3]
      --list-providers       Show list of supported cloud providers and their required .env variables
  -r, --remote <REMOTE>      Remote target directory in cloud (e.g. /Upload or /Backups/2026.09.17) [env: CLOUD_REMOTE_DIR=] [default: /Upload]
      --env-file <FILE>      Custom path to .env file
      --urls-file <PATH>     Custom path to urls.txt (local file or cloud path)
  -c, --clean                Clean (empty) target remote folder in cloud before uploading
  -f, --overwrite            Force overwrite remote files even if size and hash match
  -v, --verbose              Enable verbose / debug logging
  -h, --help                 Print help
```

### Примеры:

#### 1. Выгрузка папки бэкапа на Яндекс.Диск:
```powershell
cloud-uploader.exe -p yandex -r /Backups/2026.09.17 D:\Backups\Daily
```

#### 2. Выгрузка файла в Google Drive:
```powershell
cloud-uploader.exe -p google -r /Backups D:\Backups\database.zip
```

#### 3. Выгрузка в Cloudflare R2 / S3:
```powershell
cloud-uploader.exe -p s3 -r /daily-snapshots C:\Data\archive.tar.gz
```

#### 4. Выгрузка с предварительной очисткой папки в облаке:
```powershell
cloud-uploader.exe -p mailru -r /Backups/Latest -c D:\Backups\Daily
```

#### 5. Автономный запуск на сервере через CRON:
Файл `.env` лежит в `/opt/cloud-uploader/.env`.
В `crontab -e`:
```bash
0 3 * * * cd /opt/cloud-uploader && ./cloud-uploader >> /var/log/cloud-uploader.log 2>&1
```
*Токены и пароли защищены правами файла `.env` (`chmod 600 .env`) и не светятся в списке процессов.*
