# MiSTer Cover Scraper (ConsoleMode)

Aplicación de escritorio y consola (CLI) en **Rust** para enlazar automáticamente colecciones locales de portadas (covers) con tus carpetas de juegos de **MiSTer FPGA ConsoleMode** ([Retro-Remake/ConsoleMode_Distribution](https://github.com/Retro-Remake/ConsoleMode_Distribution)).

## Características principales

- **Escaneo recursivo de juegos e imágenes**: recorre la carpeta de juegos elegida (por ejemplo `PSX`) y todas sus subcarpetas, ignorando automáticamente `media/`, BIOS (`boot.rom`, `scph*.bin`, etc.), partidas guardadas y pistas `.bin`/`.img` duplicadas cuando ya existe un `.cue`/`.ccd`/`.gdi`.
- **Emparejamiento inteligente**:
  - Coincidencia exacta (insensible a mayúsculas).
  - Normalización de puntuación y convenciones Libretro (`_` ↔ `&` / `:`, acentos, artículos `, The`).
  - Limpieza de etiquetas de región/idioma/disco (`(USA)`, `(Europe)`, `(Disc 1)`) y conversión de números romanos (`VII` ↔ `7`).
  - Similitud difusa (Jaro-Winkler + Levenshtein + Dice por palabras) con **protección de secuelas** (evita enlazar `Game 2` con `Game 3`).
  - **Detección automática de consola** (`Solo misma consola` activado por defecto): distingue extensiones y carpetas de sistema (ej. GBA vs PSX vs GB/GBC vs SNES) para no asignar portadas de otra consola aunque compartan el mismo nombre.
  - Preferencia automática por región coincidente cuando hay varias portadas disponibles.
- **Formato compatible con ConsoleMode**:
  - Crea la carpeta `media/` dentro de la carpeta de juegos (por defecto `<juegos>/media/`, o por cada subcarpeta si se activa la opción).
  - Guarda únicamente las portadas faltantes con el **nombre exacto del archivo del juego** (sin extensión del ROM) en formato `.png` (convirtiendo `.jpg`, `.jpeg`, `.webp`, `.bmp`, `.gif` automáticamente).
- **Conexión local y remota por internet**:
  - **Local / Tarjeta SD / SMB de Windows**: por ejemplo `D:\games\PSX` o `\\mister\fat\games\PSX`.
  - **SFTP / SSH (puerto 22)**: accede directamente al almacenamiento de tu MiSTer FPGA tanto en red local (`mister.local` o IP LAN) como a través de internet (Tailscale, WireGuard o SSH expuesto), con autenticación por contraseña (por defecto `root` / `1` en MiSTer) o clave privada SSH y explorador visual de carpetas remotas.
- **Interfaz gráfica (GUI) y modo consola (CLI)**:
  - Vista previa de portadas, filtro de dudosos/faltantes, ajuste en vivo del umbral de similitud, top 5 de sugerencias y buscador para asignación manual.
- **Carga masiva de juegos (`Cargar Juegos (Bulk)` / subcomando `upload`)**:
  - Sube archivos individuales, **comprimidos (`.zip`, `.7z`, `.rar`)** o carpetas completas en lote (incluidas carpetas con `.cue` + múltiples pistas `.bin`, `.gdi` o `.m3u` multidisco) arrastrando y soltando o desde el selector.
  - **Lectura interna de `.zip` / `.7z` / `.rar`**: inspecciona las cabeceras sin descomprimir primero para clasificar ROMs sueltos y bundles de disco (conservando los `.zip` de Arcade/MAME intactos y soportando volúmenes `.partXX.rar`), extrae al vuelo con motores nativos (y respaldo automático con `7-Zip` / `UnRAR` / `bsdtar`).
  - **Conversión opcional BIN/CUE (y GDI) → CHD**: transforma discos `.cue`/`.bin` y `.gdi` a un solo `.chd` v5 usando `chd-rs` integrado (sin requerir `chdman`, aunque acepta `chdman.exe` como respaldo opcional), limpiando UTF-8 BOM/rutas del `.cue` y reescribiendo playlists `.m3u` multidisco.
  - Detecta la consola de cada juego, enruta automáticamente a su carpeta correspondiente (`PSX/`, `GBA/`, `SNES/`, `MegaDrive/`, etc.) y crea/asigna su carátula `.png` en `media/` mientras transmite por streaming sin saturar la memoria RAM.

## Compilación

Requisitos: Rust toolchain (`rustc` / `cargo`, edition 2024) con soporte MSVC de Visual Studio en Windows (compila todo en local, incluyendo `chd-rs` y `unrar-ng`). No requiere `chdman`, `7z`, OpenSSL, CMake ni NASM externos para compilar o usar las funciones básicas.

```powershell
cargo build --release
```

El binario compilado queda en `target\release\mister-cover-scraper.exe`.

## Uso rápido

### 1. Interfaz gráfica de escritorio (GUI)

Ejecuta sin argumentos (o con el subcomando `gui`):

```powershell
cargo run --release
```

1. Elige **Carpeta Local / SMB** o **MiSTer Remoto (SFTP / SSH)**.
2. Selecciona la carpeta de juegos (ej. `PSX` o `/media/fat/games/PSX` en la MiSTer) y la carpeta de imágenes (ej. `sony playstation`).
3. Pulsa **Analizar y Emparejar** para revisar los enlaces sugeridos, previsualizar portadas y corregir manualmente cualquier juego dudoso.
4. Pulsa **Aplicar Covers Seleccionados** para crear `media/` y copiar o subir las portadas faltantes.

### 2. Línea de comandos (CLI)

#### Carpeta local o recurso compartido SMB

```powershell
# Simulación previa (--dry-run) sin escribir archivos
cargo run --release -- sync --games "D:\MiSTer\games\PSX" --images "D:\Covers\sony playstation" --dry-run

# Ejecutar copia y conversión a PNG
cargo run --release -- sync --games "D:\MiSTer\games\PSX" --images "D:\Covers\sony playstation"
```

#### Remoto vía SFTP / SSH (LAN, Tailscale o Internet)

```powershell
# Listar carpetas disponibles en la MiSTer
cargo run --release -- list-remote --sftp 192.168.1.50 --path /media/fat/games

# Sincronizar covers de PSX directamente hacia la MiSTer
cargo run --release -- sync --sftp 192.168.1.50 --games /media/fat/games/PSX --images "D:\Covers\sony playstation"

# Usando clave privada SSH y contraseña por variable de entorno si aplica
cargo run --release -- sync --sftp mi-mister.tailnet.ts.net --key "C:\Users\TuUsuario\.ssh\id_ed25519" --games /media/fat/games/PSX --images "D:\Covers\sony playstation"
```

Opciones útiles de `sync`:
- `--threshold 0.78`: umbral mínimo de similitud (`0.0` a `1.0`).
- `--overwrite`: sobrescribe portadas que ya existan en `media/`.
- `--keep-format`: conserva el formato original (`.jpg`, `.webp`, etc.) sin convertir a `.png`.
- `--per-subfolder-media`: crea `media/` dentro de cada subcarpeta en lugar de `<juegos>/media/`.
- `--no-recursive-images`: busca imágenes solo en la raíz de `--images`.
- `--allow-cross-system`: permite emparejar portadas detectadas como de otra consola (por defecto se bloquean).

#### Carga masiva de juegos, comprimidos y BIN/CUE → CHD (`upload`)

Permite subir en lote archivos sueltos, comprimidos (`.zip`, `.7z`, `.rar`) y carpetas completas con `.cue`/`.bin`, `.gdi` o `.m3u`, enrutando cada juego a su carpeta de consola bajo `--games-root`, convirtiendo discos a `.chd` si se indica `--convert-to-chd` y creando su portada `.png` en `media/`:

```powershell
# Subir en lote por SFTP convirtiendo .7z/.cue/.bin a CHD al vuelo
cargo run --release -- upload `
  --sftp 192.168.1.50 `
  --games-root /media/fat/games `
  --images "D:\Covers" `
  --convert-to-chd `
  --sources "D:\Descargas\Golden Sun (USA).zip" "D:\Descargas\Castlevania - Symphony of the Night (USA).7z" `
  --dry-run
```

Opciones clave de `upload`:
- `--convert-to-chd`: convierte discos `.cue`/`.bin` y `.gdi` (sueltos, en carpeta o dentro de `.zip`/`.7z`/`.rar`) a `.chd` usando `chd-rs` integrado antes de subir.
- `--chdman <RUTA>`: ruta opcional a `chdman.exe` como respaldo alternativo.
- `--temp-dir <CARPETA>`: carpeta temporal personalizada para extraer archivos grandes o generar `.chd` (útil si `C:` tiene poco espacio).
- `--system <CONSOLA>`: consola por defecto para juegos de extensión ambigua (`.chd`, `.cue`, `.iso`, `.bin`).
- `--overwrite-games` / `--overwrite-covers`: sobrescribe juegos o portadas ya existentes en la MiSTer.
- `--wrap-disc-in-folder`: envuelve discos sueltos (`.chd`/`.iso`/`.m3u`) dentro de `<Consola>/<Juego>/<archivo>`.

## Consejo para ConsoleMode en MiSTer FPGA

Después de añadir o subir nuevas portadas a `media/`, abre **Settings → Game Library** en ConsoleMode y ejecuta **Rebuild Game Database** y/o **Optimize Artwork** si el frontend no muestra las nuevas imágenes de inmediato.

## Manual web interactivo

Abre [docs/manual.html](docs/manual.html) directamente en cualquier navegador (funciona sin conexión a internet) para ver:

- Demostración visual de la estructura de carpetas `media/` para ConsoleMode.
- Guía interactiva de cada zona de la interfaz gráfica (GUI).
- Simulador en vivo del algoritmo de emparejamiento (Libretro `_`, números romanos, `, The`, regiones y guardia de secuelas).
- Generador interactivo de comandos CLI para modo local/SMB y SFTP/SSH.
