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
  - **Descubrimiento automático de la MiSTer**: botón **🔎 Buscar MiSTer** en la GUI y subcomando `discover` en CLI que escanean la red local, detectan hosts con SSH/SFTP y verifican el acceso a `/media/fat`.
  - **Detección y eliminación de duplicados y boot/demo**: pestaña **🧹 Duplicados / Limpieza** en la GUI y subcomandos `duplicates` / `clean-boot` en CLI. Agrupa juegos por nombre normalizado (región, tag de disco, acentos, artículos y números romanos se ignoran), recomienda qué copia conservar y elimina el resto. También detecta archivos cosméticos `mister-boot` / `mister-demo` (uberyoji Boot ROMs) —preguntando tras analizar o desde la pestaña de limpieza— sin tocar nunca las BIOS reales (`boot.rom`–`boot3.rom`). En local mueve los archivos a una papelera recuperable; por SFTP los elimina tras confirmación. Al borrar una hoja `.cue`/`.ccd`/`.gdi` también elimina sus pistas `.bin`/`.img` huérfanas.
  - **AppImage para Linux**: además del binario y del `.exe` de Windows, las releases publican un AppImage portable.
- **Interfaz gráfica (GUI) y modo consola (CLI)**:
  - Vista previa de portadas, filtro de dudosos/faltantes, ajuste en vivo del umbral de similitud, top 5 de sugerencias y buscador para asignación manual.
- **Carga masiva de juegos (`Cargar Juegos (Bulk)` / subcomando `upload`)**:
  - Sube archivos individuales, **comprimidos (`.zip`, `.7z`, `.rar`)** o carpetas completas en lote (incluidas carpetas con `.cue` + múltiples pistas `.bin`, `.gdi` o `.m3u` multidisco) arrastrando y soltando o desde el selector.
  - **Lectura interna de `.zip` / `.7z` / `.rar`**: inspecciona las cabeceras sin descomprimir primero para clasificar ROMs sueltos y bundles de disco (conservando los `.zip` de Arcade/MAME intactos y soportando volúmenes `.partXX.rar`), extrae al vuelo con motores nativos (y respaldo automático con `7-Zip` / `UnRAR` / `bsdtar`).
  - **Conversión opcional BIN/CUE (y GDI) → CHD**: transforma discos `.cue`/`.bin` y `.gdi` a un solo `.chd` v5 usando `chd-rs` integrado (sin requerir `chdman`, aunque acepta `chdman` como respaldo opcional), limpiando UTF-8 BOM/rutas del `.cue` y reescribiendo playlists `.m3u` multidisco.
  - Detecta la consola de cada juego, enruta automáticamente a su carpeta correspondiente (`PSX/`, `GBA/`, `SNES/`, `MegaDrive/`, etc.) y crea/asigna su carátula `.png` en `media/` mientras transmite por streaming sin saturar la memoria RAM.

## Compilación

Requisitos: Rust toolchain (`rustc` / `cargo`, edition 2024). El proyecto compila `chd-rs` y `unrar-ng` localmente; no requiere `chdman`, `7z`, OpenSSL, CMake ni NASM externos para las funciones básicas.

### Windows

Requiere soporte MSVC de Visual Studio.

```powershell
cargo build --release
```

El binario compilado queda en `target\release\mister-cover-scraper.exe`.

### Linux (CachyOS / Arch)

Instala las dependencias de sistema necesarias para compilar la GUI (eframe/egui/winit/rfd) y las herramientas nativas:

```bash
sudo pacman -S --needed base-devel rust clang pkgconf gtk3 libxcb libxkbcommon libxkbcommon-x11 wayland \
  xdg-desktop-portal fontconfig freetype2
```

> En CachyOS también funciona con `rustup` si prefieres gestionar el toolchain manualmente. `xdg-desktop-portal` (y un backend como `xdg-desktop-portal-kde` o `xdg-desktop-portal-gtk`) es necesario para los diálogos de selección de archivos en entornos sandbox/portal.

Compila:

```bash
cargo build --release
```

El binario compilado queda en `target/release/mister-cover-scraper`.

### AppImage (Linux)

Las releases incluyen `mister-cover-scraper-linux-x86_64.AppImage` (además del binario y del `.exe` de Windows). El workflow de GitHub Actions se dispara al empujar un tag `v*`. Para generarlo localmente:

```bash
sudo pacman -S --needed patchelf desktop-file-utils curl librsvg gobject-introspection \
  glib2 gdk-pixbuf2 squashfs-tools
cargo build --release --locked
bash packaging/linux/build-appimage.sh
```

El AppImage se genera como `mister-cover-scraper-linux-x86_64.AppImage` en la raíz del proyecto.

Para ejecutarlo:

```bash
chmod +x mister-cover-scraper-linux-x86_64.AppImage
./mister-cover-scraper-linux-x86_64.AppImage
```

Si tu sistema no tiene FUSE 2 (`sudo pacman -S --needed fuse2`), puedes ejecutarlo sin montarlo:

```bash
./mister-cover-scraper-linux-x86_64.AppImage --appimage-extract-and-run
```

## Uso rápido

### 1. Interfaz gráfica de escritorio (GUI)

Ejecuta sin argumentos (o con el subcomando `gui`):

```powershell
cargo run --release
```

1. Elige **Carpeta Local / SMB** o **MiSTer Remoto (SFTP / SSH)**.
2. Si usas SFTP, pulsa **🔎 Buscar MiSTer** para escanear la red local y seleccionar la IP detectada.
3. Selecciona la carpeta de juegos (ej. `PSX` o `/media/fat/games/PSX` en la MiSTer) y la carpeta de imágenes (ej. `sony playstation`).
4. Pulsa **Analizar y Emparejar** para revisar los enlaces sugeridos, previsualizar portadas y corregir manualmente cualquier juego dudoso.
5. Pulsa **Aplicar Covers Seleccionados** para crear `media/` y copiar o subir las portadas faltantes.

### 2. Línea de comandos (CLI)

#### Buscar la IP de la MiSTer (`discover`)

```bash
# Escanear la red local detectada automáticamente
cargo run --release -- discover

# Escanear una subred concreta y mostrar JSON
cargo run --release -- discover --subnet 192.168.1.0/24 --json

# Solo detectar hosts con SSH abierto, sin verificar por SFTP
cargo run --release -- discover --no-verify
```

El escáner comprueba el puerto SSH (22 por defecto), lee el banner y, si la verificación está activada, intenta autenticarse por SFTP y comprobar que `/media/fat` sea accesible (por defecto usuario `root` y contraseña `1`). Opciones útiles: `--subnet`, `--port`, `--user`, `--password`, `--key`, `--timeout-ms`, `--concurrency`, `--json`.

> Úsalo solo en tu propia red local. Si no quieres probar credenciales SSH, añade `--no-verify`.

#### Detectar y eliminar juegos duplicados (`duplicates`)

En la **GUI**, abre la pestaña **🧹 Duplicados / Limpieza**, elige la carpeta de juegos (o la ruta SFTP), pulsa **Buscar duplicados y boot/demo**, selecciona con el círculo qué copia conservar en cada grupo y pulsa **Eliminar duplicados** (se pedirá confirmación).

En **CLI**:

```bash
# 1) Listar grupos (marca KEEP la copia recomendada)
cargo run --release -- duplicates --games "/run/media/$USER/MiSTer/games"

# 2) Simular: conservar una copia y borrar el resto del grupo
cargo run --release -- duplicates --games "/run/media/$USER/MiSTer/games" \
  --keep "/run/media/$USER/MiSTer/games/PSX/Final Fantasy VII (USA).chd" --dry-run

# 3) Aplicar en SFTP (borrado permanente)
cargo run --release -- duplicates --sftp 192.168.1.50 --games /media/fat/games \
  --keep "/media/fat/games/PSX/Final Fantasy VII (USA).chd" --yes

# 4) Borrar rutas concretas y ver resultados en JSON
cargo run --release -- duplicates --games "$HOME/MiSTer/games" --json
```

Opciones: `--games` (raíz), `--sftp`/`--port`/`--user`/`--password`/`--key`, `--keep <RUTA>...`, `--delete <RUTA>...`, `--dry-run`, `--yes`, `--json`.

> En local los duplicados se mueven a `<raíz>/.duplicates_trash/<timestamp>/` para poder recuperarlos. En SFTP no hay papelera: la eliminación es permanente y por eso se pide confirmación (o `--yes`).

#### Limpiar archivos `mister-boot` y `mister-demo` (`clean-boot`)

En la **GUI**, al escanear portadas o usar la pestaña **🧹 Duplicados / Limpieza**, si aparecen archivos cosméticos `mister-boot` / `mister-demo` (de *uberyoji Boot ROMs*) se abre un diálogo preguntando cuáles deseas eliminar (las BIOS reales como `boot.rom`–`boot3.rom` nunca se muestran ni se tocan).

En **CLI**:

```bash
# 1) Listar archivos mister-boot / mister-demo sin modificar nada
cargo run --release -- clean-boot --games "/run/media/$USER/MiSTer/games" --list-only

# 2) Simular limpieza en local
cargo run --release -- clean-boot --games "/run/media/$USER/MiSTer/games" --dry-run

# 3) Preguntar y eliminar por SFTP (o añadir --yes para confirmar directo)
cargo run --release -- clean-boot --sftp 192.168.1.50 --games /media/fat/games

# 4) Ver lista en JSON
cargo run --release -- clean-boot --games "$HOME/MiSTer/games" --json
```

Opciones: `--games`, `--sftp`/`--port`/`--user`/`--password`/`--key`, `--list-only`, `--dry-run`, `--yes`, `--json`.

#### Carpeta local, recurso compartido SMB o montaje Linux

```bash
# Simulación previa (--dry-run) sin escribir archivos
cargo run --release -- sync --games "/run/media/$USER/MiSTer/games/PSX" --images "$HOME/Covers/sony playstation" --dry-run

# Ejecutar copia y conversión a PNG
cargo run --release -- sync --games "/run/media/$USER/MiSTer/games/PSX" --images "$HOME/Covers/sony playstation"
```

<details>
<summary>Ejemplos en PowerShell (Windows)</summary>

```powershell
cargo run --release -- sync --games "D:\MiSTer\games\PSX" --images "D:\Covers\sony playstation" --dry-run
cargo run --release -- sync --games "D:\MiSTer\games\PSX" --images "D:\Covers\sony playstation"
```

</details>

#### Remoto vía SFTP / SSH (LAN, Tailscale o Internet)

```bash
# Listar carpetas disponibles en la MiSTer
cargo run --release -- list-remote --sftp 192.168.1.50 --path /media/fat/games

# Sincronizar covers de PSX directamente hacia la MiSTer
cargo run --release -- sync --sftp 192.168.1.50 --games /media/fat/games/PSX --images "$HOME/Covers/sony playstation"

# Usando clave privada SSH
cargo run --release -- sync --sftp mi-mister.tailnet.ts.net --key "$HOME/.ssh/id_ed25519" --games /media/fat/games/PSX --images "$HOME/Covers/sony playstation"
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

```bash
# Subir en lote por SFTP convirtiendo .7z/.cue/.bin a CHD al vuelo
cargo run --release -- upload \
  --sftp 192.168.1.50 \
  --games-root /media/fat/games \
  --images "$HOME/Covers" \
  --convert-to-chd \
  --sources "$HOME/Descargas/Golden Sun (USA).zip" "$HOME/Descargas/Castlevania - Symphony of the Night (USA).7z" \
  --dry-run
```

Opciones clave de `upload`:
- `--convert-to-chd`: convierte discos `.cue`/`.bin` y `.gdi` (sueltos, en carpeta o dentro de `.zip`/`.7z`/`.rar`) a `.chd` usando `chd-rs` integrado antes de subir.
- `--chdman <RUTA>`: ruta opcional a `chdman` como respaldo alternativo.
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
