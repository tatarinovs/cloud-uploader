# Cloud Uploader

Утилита для выгрузки бэкапов в облачные хранилища и автоматического зеркалирования релизов GitHub и файлов по ссылкам. Один самодостаточный бинарник (Windows / Linux, в том числе ARM64), рассчитанный на запуск из планировщика.

---

## ✨ Возможности

**Провайдеры**

| Ключ `-p` | Хранилище | Проверка «файл не изменился» |
|---|---|---|
| `yandex` | Яндекс.Диск (REST API) | размер + MD5 |
| `mailru` | Облако Mail.ru (WebDAV) | размер + дата изменения |
| `google` | Google Drive (API v3, resumable upload) | размер + MD5 |
| `webdav` | Nextcloud, ownCloud, Synology и любой WebDAV | размер + дата (MD5 при `WEBDAV_ETAG_IS_MD5=true`) |
| `s3` | AWS S3, Cloudflare R2, MinIO, Yandex Object Storage | размер + MD5 (для multipart — дата) |

**Режим 1 — выгрузка файла или папки.** Дерево каталогов воссоздаётся в облаке, неизменённые файлы пропускаются. Если у провайдера нет MD5, сравнивается дата изменения: это важно для многотомных архивов, где все тома одного размера. Загрузки идут параллельно (`-j`).

**Режим 2 — зеркалирование ссылок** (если путь не указан). Список берётся из `urls.txt`:
1. `--urls-file <PATH>`: локальный файл или путь в облаке;
2. `./urls.txt`, затем `urls.txt` рядом с бинарником;
3. `{remote_dir}/urls.txt` в облаке, затем `/urls.txt`.

Поддерживаемые виды ссылок:
* `https://github.com/owner/repo` — последний релиз; `.../releases/tag/<tag>` — конкретный релиз. Ассеты, подходящие под `--assets`, сохраняются в `{remote}/{repo}/[tag] имя_файла`. Старые версии того же файла удаляются **только после** успешной загрузки новой. Чужие файлы не трогаются.
* Любые прямые ссылки: файл скачивается и заливается, если его MD5 отличается от облачного.

**Надёжность**
* Повторы с экспоненциальной задержкой при сетевых сбоях, 5xx и 429 (с учётом `Retry-After`), в том числе для самих загрузок.
* Без общего тайм-аута на передачу: многогигабайтные файлы не обрываются. Зависшие соединения отсекаются keep-alive и тайм-аутом, пропорциональным размеру.
* Ограниченное потребление памяти: файлы передаются потоком. S3 загружает файлы больше 64 МБ частями (multipart), Google Drive продолжает прерванную загрузку с места обрыва.
* Перед `--clean` проверяются локальный путь и список ссылок, а облачный `urls.txt` не удаляется. Очистка корня `/` запрещена.
* `--dry-run` показывает, что будет загружено или удалено, ничего не меняя.
* Коды возврата для скриптов: `0` успех, `1` фатальная ошибка, `2` неверные аргументы, `3` часть файлов не удалась, `130` прервано Ctrl+C.

---

## 🚀 Сборка

Требуется Rust 1.80+.

```cmd
build.bat          # Windows .exe и Linux ARM64
build.bat exe      # только Windows .exe
build.bat arm64    # только Linux ARM64 (через zig)
```
Готовые файлы складываются в `dist/`.

Ручная сборка:
```bash
cargo build --release                                          # текущая платформа
cargo build --release --target x86_64-unknown-linux-musl       # Linux x86_64
cargo zigbuild --release --target aarch64-unknown-linux-musl   # Linux ARM64
cargo test                                                     # тесты
```

---

## ⚙️ Настройка

Скопируйте [.env.example](.env.example) в `.env` в текущую папку или рядом с программой либо укажите `--env-file`. Системные переменные окружения имеют приоритет над `.env`. Значения `CLOUD_PROVIDER`, `CLOUD_REMOTE_DIR`, `CLOUD_JOBS` из `.env` работают как значения по умолчанию для ключей командной строки.

`cloud-uploader --list-providers` выводит переменные, нужные каждому провайдеру.

**Google Drive.** Access token живёт около часа, поэтому для регулярных бэкапов укажите `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET` и `GOOGLE_REFRESH_TOKEN` (OAuth-клиент типа Desktop, scope `https://www.googleapis.com/auth/drive`). Токены будут обновляться автоматически.

**S3.** Используются path-style запросы (`{endpoint}/{bucket}/{key}`). В S3 нет настоящих папок, поэтому пустые каталоги не создаются.

---

## 📖 Использование

```text
cloud-uploader [OPTIONS] [LOCAL_PATH]

  -p, --provider <PROVIDER>  yandex | mailru | google | webdav | s3   [env: CLOUD_PROVIDER] [default: yandex]
  -r, --remote <REMOTE>      Папка в облаке                           [env: CLOUD_REMOTE_DIR] [default: /Upload]
      --env-file <FILE>      Путь к .env
      --urls-file <PATH>     Список ссылок: локальный файл или путь в облаке  [env: CLOUD_URLS_FILE]
  -c, --clean                Очистить папку в облаке перед загрузкой
  -f, --overwrite            Загружать даже неизменённые файлы
  -n, --dry-run              Только показать, что будет сделано
  -j, --jobs <JOBS>          Параллельные загрузки, 1..32              [env: CLOUD_JOBS] [default: 2]
      --assets <REGEX>       Какие ассеты релизов скачивать            [default: (?i)\.(apk|exe|zip|tar\.gz)$]
      --list-providers       Список провайдеров и их переменных
  -v, --verbose / -q, --quiet
```

### Примеры

```powershell
# Папка бэкапа на Яндекс.Диск
cloud-uploader.exe -p yandex -r /Backups/2026.09.17 D:\Backups\Daily

# Сначала посмотреть, что изменится
cloud-uploader.exe -p mailru -r /Backups/Latest -c -n D:\Backups\Daily

# Cloudflare R2, 4 параллельные загрузки
cloud-uploader.exe -p s3 -r /daily-snapshots -j 4 C:\Data

# Зеркалирование релизов, только .apk
cloud-uploader.exe -p google -r /Mirror --assets "(?i)\.apk$"
```

### Запуск по расписанию (cron)

```bash
0 3 * * * cd /opt/cloud-uploader && ./cloud-uploader -q /srv/backups >> /var/log/cloud-uploader.log 2>&1 || echo "backup failed: $?"
```
Ненулевой код возврата означает проблему: `3` — часть файлов не загрузилась (подробности в логе). Защитите `.env` правами `chmod 600 .env`. Секреты никогда не передаются через аргументы командной строки и не видны в списке процессов.

> **Git Bash на Windows** превращает аргументы вида `/Backups` в пути Windows. Используйте `MSYS_NO_PATHCONV=1` или запускайте из PowerShell/cmd.
