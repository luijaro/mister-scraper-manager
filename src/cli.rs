use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Args;

use crate::config::{MediaPlacement, SftpAuthMode, SftpConfig, SyncOptions};
use crate::discover::{self, DiscoverOptions};
use crate::duplicates;
use crate::models::MatchStatus;
use crate::scanner;
use crate::storage::StorageClient;
use crate::sync::{self, ApplyEvent};
use crate::systems::SystemId;
use crate::uploader::{self, UploadEvent, UploadOptions, UploadStatus};

/// Arguments for the `sync` CLI subcommand.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Carpeta de juegos (ruta local/SMB o ruta remota en la MiSTer si se usa --sftp)
    #[arg(long)]
    pub games: String,

    /// Carpeta local con las imágenes/covers
    #[arg(long)]
    pub images: PathBuf,

    /// Host o IP de la MiSTer FPGA para conectar vía SFTP/SSH (ej. 192.168.1.50 o mister.local)
    #[arg(long)]
    pub sftp: Option<String>,

    /// Puerto SSH/SFTP (por defecto 22)
    #[arg(long, default_value_t = 22)]
    pub port: u16,

    /// Usuario SSH de la MiSTer (por defecto root)
    #[arg(long, default_value = "root")]
    pub user: String,

    /// Contraseña SSH (si se omite sin --key, usa "1" por defecto en MiSTer)
    #[arg(long, env = "MISTER_SSH_PASSWORD")]
    pub password: Option<String>,

    /// Ruta a clave privada SSH para autenticación sin contraseña
    #[arg(long)]
    pub key: Option<PathBuf>,

    /// Frase de paso de la clave privada SSH (si tiene)
    #[arg(long, env = "MISTER_SSH_PASSPHRASE")]
    pub passphrase: Option<String>,

    /// Umbral mínimo de similitud entre 0.0 y 1.0 (por defecto 0.78)
    #[arg(long, default_value_t = 0.78)]
    pub threshold: f64,

    /// Simula el proceso sin crear carpetas ni copiar/subir archivos
    #[arg(long)]
    pub dry_run: bool,

    /// Sobrescribe covers ya existentes en la carpeta media
    #[arg(long)]
    pub overwrite: bool,

    /// Mantiene la extensión original de la imagen en lugar de convertir a PNG
    #[arg(long)]
    pub keep_format: bool,

    /// Crea una carpeta media/ dentro de cada subcarpeta en lugar de usar <juegos>/media
    #[arg(long)]
    pub per_subfolder_media: bool,

    /// No buscar imágenes dentro de subcarpetas de --images
    #[arg(long)]
    pub no_recursive_images: bool,

    /// Permite enlazar portadas de otra consola si tienen el mismo nombre (por defecto se bloquea)
    #[arg(long)]
    pub allow_cross_system: bool,
}

/// Arguments for the `list-remote` CLI subcommand.
#[derive(Debug, Args)]
pub struct ListRemoteArgs {
    /// Host o IP de la MiSTer FPGA
    #[arg(long)]
    pub sftp: String,

    /// Puerto SSH/SFTP (por defecto 22)
    #[arg(long, default_value_t = 22)]
    pub port: u16,

    /// Usuario SSH (por defecto root)
    #[arg(long, default_value = "root")]
    pub user: String,

    /// Contraseña SSH (si se omite sin --key, usa "1")
    #[arg(long, env = "MISTER_SSH_PASSWORD")]
    pub password: Option<String>,

    /// Ruta a clave privada SSH
    #[arg(long)]
    pub key: Option<PathBuf>,

    /// Frase de paso de la clave privada SSH
    #[arg(long, env = "MISTER_SSH_PASSPHRASE")]
    pub passphrase: Option<String>,

    /// Ruta remota a listar en la MiSTer
    #[arg(long, default_value = "/media/fat/games")]
    pub path: String,
}

/// Arguments for the `discover` CLI subcommand.
#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// Subred a escanear en formato CIDR (ej. 192.168.1.0/24). Si se omite, se detecta la red local.
    #[arg(long)]
    pub subnet: Option<String>,

    /// Puerto SSH a comprobar
    #[arg(long, default_value_t = 22)]
    pub port: u16,

    /// Usuario SSH usado para verificar que el host es una MiSTer
    #[arg(long, default_value = "root")]
    pub user: String,

    /// Contraseña SSH para la verificación (por defecto "1" en MiSTer)
    #[arg(long, env = "MISTER_SSH_PASSWORD")]
    pub password: Option<String>,

    /// Ruta a clave privada SSH para la verificación
    #[arg(long)]
    pub key: Option<PathBuf>,

    /// Frase de paso de la clave privada SSH
    #[arg(long, env = "MISTER_SSH_PASSPHRASE")]
    pub passphrase: Option<String>,

    /// Tiempo máximo por host en milisegundos
    #[arg(long, default_value_t = 400)]
    pub timeout_ms: u64,

    /// No verificar por SFTP; solo detectar hosts con el puerto SSH abierto
    #[arg(long)]
    pub no_verify: bool,

    /// Número de hosts sondeados en paralelo
    #[arg(long, default_value_t = 128)]
    pub concurrency: usize,

    /// Imprime los resultados en JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for the `duplicates` CLI subcommand.
#[derive(Debug, Args)]
pub struct DuplicatesArgs {
    /// Carpeta raíz de juegos a escanear (local o remota si se usa --sftp)
    #[arg(long)]
    pub games: String,

    /// Host o IP de la MiSTer para escanear por SFTP/SSH
    #[arg(long)]
    pub sftp: Option<String>,

    #[arg(long, default_value_t = 22)]
    pub port: u16,

    #[arg(long, default_value = "root")]
    pub user: String,

    #[arg(long, env = "MISTER_SSH_PASSWORD")]
    pub password: Option<String>,

    #[arg(long)]
    pub key: Option<PathBuf>,

    #[arg(long, env = "MISTER_SSH_PASSPHRASE")]
    pub passphrase: Option<String>,

    /// Muestra los resultados en JSON (cuando no se pide eliminación)
    #[arg(long)]
    pub json: bool,

    /// Simula la eliminación sin borrar ni mover archivos
    #[arg(long)]
    pub dry_run: bool,

    /// Conserva estas rutas y elimina el resto de sus grupos de duplicados
    #[arg(long, num_args = 1..)]
    pub keep: Vec<String>,

    /// Elimina estas rutas concretas (deben pertenecer a un grupo de duplicados)
    #[arg(long, num_args = 1..)]
    pub delete: Vec<String>,

    /// Confirma la eliminación sin preguntar
    #[arg(long)]
    pub yes: bool,
}

/// Arguments for the `upload` CLI subcommand.
#[derive(Debug, Args)]
pub struct UploadArgs {
    /// Uno o más archivos (.gba, .zip, .7z, .rar, .cue...) o carpetas de juegos (incluidas carpetas con .cue/.bin) a cargar en lote
    #[arg(long, num_args = 1.., required = true)]
    pub sources: Vec<PathBuf>,

    /// Carpeta raíz de juegos de la MiSTer (ej. D:\MiSTer\games o /media/fat/games)
    #[arg(long)]
    pub games_root: String,

    /// Carpeta local con portadas (opcional; si se indica, asigna carátulas automáticamente)
    #[arg(long)]
    pub images: Option<PathBuf>,

    /// Consola por defecto para juegos ambiguos (ej. PSX, GBA, Saturn, MegaCD)
    #[arg(long)]
    pub system: Option<String>,

    /// Host o IP de la MiSTer FPGA para subir vía SFTP/SSH
    #[arg(long)]
    pub sftp: Option<String>,

    /// Puerto SSH/SFTP (por defecto 22)
    #[arg(long, default_value_t = 22)]
    pub port: u16,

    /// Usuario SSH de la MiSTer (por defecto root)
    #[arg(long, default_value = "root")]
    pub user: String,

    /// Contraseña SSH (si se omite sin --key, usa "1" por defecto en MiSTer)
    #[arg(long, env = "MISTER_SSH_PASSWORD")]
    pub password: Option<String>,

    /// Ruta a clave privada SSH
    #[arg(long)]
    pub key: Option<PathBuf>,

    /// Frase de paso de la clave privada SSH
    #[arg(long, env = "MISTER_SSH_PASSPHRASE")]
    pub passphrase: Option<String>,

    /// Umbral mínimo de similitud para emparejar carátulas (por defecto 0.78)
    #[arg(long, default_value_t = 0.78)]
    pub threshold: f64,

    /// Convierte automáticamente discos .bin/.cue (y .gdi) a .chd antes de cargarlos
    #[arg(long)]
    pub convert_to_chd: bool,

    /// Ruta opcional a chdman (por defecto se usa el conversor chd-rs integrado)
    #[arg(long)]
    pub chdman: Option<PathBuf>,

    /// Carpeta temporal personalizada para extraer archivos grandes o generar .chd
    #[arg(long)]
    pub temp_dir: Option<PathBuf>,

    /// Sobrescribe archivos de juego ya existentes en el destino
    #[arg(long)]
    pub overwrite_games: bool,

    /// Sobrescribe carátulas ya existentes en media/
    #[arg(long)]
    pub overwrite_covers: bool,

    /// Mantiene el formato original de la carátula en lugar de convertir a PNG
    #[arg(long)]
    pub keep_format: bool,

    /// Envuelve discos sueltos (.chd/.iso/.m3u) dentro de <Consola>/<Juego>/<archivo>
    #[arg(long)]
    pub wrap_disc_in_folder: bool,

    /// Crea media/ dentro de cada subcarpeta de juego cuando corresponda
    #[arg(long)]
    pub per_subfolder_media: bool,

    /// No buscar imágenes dentro de subcarpetas de --images
    #[arg(long)]
    pub no_recursive_images: bool,

    /// Permite enlazar portadas de otra consola si comparten nombre
    #[arg(long)]
    pub allow_cross_system: bool,

    /// Simula la carga y asignación sin crear carpetas ni escribir archivos
    #[arg(long)]
    pub dry_run: bool,
}

async fn connect_sftp_client(
    host: String,
    port: u16,
    username: String,
    password: Option<String>,
    key: Option<PathBuf>,
    passphrase: Option<String>,
    remote_path: String,
) -> Result<StorageClient> {
    let (auth_mode, resolved_password, key_path) = if let Some(key_buf) = key {
        (
            SftpAuthMode::PrivateKey,
            String::new(),
            key_buf.to_string_lossy().to_string(),
        )
    } else {
        let pwd = match password {
            Some(p) => p,
            None => {
                println!(
                    "Info: No se indicó --password ni --key; usando contraseña por defecto de MiSTer (\"1\")."
                );
                "1".to_string()
            }
        };
        (SftpAuthMode::Password, pwd, String::new())
    };

    let cfg = SftpConfig {
        host,
        port,
        username,
        auth_mode,
        password: resolved_password,
        private_key_path: key_path,
        passphrase: passphrase.unwrap_or_default(),
        remote_path,
    };

    StorageClient::connect_sftp(&cfg).await
}

/// Runs the `list-remote` CLI command to browse remote folders on MiSTer FPGA.
pub async fn run_list_remote(args: ListRemoteArgs) -> Result<()> {
    println!("Conectando a {}:{} por SFTP...", args.sftp, args.port);
    let storage = connect_sftp_client(
        args.sftp,
        args.port,
        args.user,
        args.password,
        args.key,
        args.passphrase,
        args.path.clone(),
    )
    .await?;

    let dirs = storage.list_subdirs(&args.path).await?;
    println!("Subcarpetas en {} ({}):", args.path, dirs.len());
    for d in dirs {
        println!("  [DIR] {d}");
    }
    storage.close().await;
    Ok(())
}

/// Runs the `discover` CLI command to find MiSTer devices on the local network.
pub async fn run_discover(args: DiscoverArgs) -> Result<()> {
    let options = DiscoverOptions {
        subnet: args.subnet.clone(),
        port: args.port,
        timeout: Duration::from_millis(args.timeout_ms.clamp(50, 10_000)),
        verify: !args.no_verify,
        sftp_user: args.user.clone(),
        sftp_password: args.password.clone(),
        sftp_key: args.key.clone(),
        sftp_passphrase: args.passphrase.clone(),
        concurrency: args.concurrency,
    };

    let scope = args
        .subnet
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("red local detectada");
    println!("Buscando MiSTer en {scope} (puerto {})...", args.port);

    let cancel = AtomicBool::new(false);
    let results = discover::discover(&options, &cancel, |progress| {
        if progress.scanned == progress.total || progress.scanned % 32 == 0 {
            print!(
                "\rEscaneados {}/{} hosts · {} candidato(s)...",
                progress.scanned, progress.total, progress.found
            );
            let _ = std::io::stdout().flush();
        }
    })
    .await?;
    println!();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
        return Ok(());
    }

    if results.is_empty() {
        println!("No se encontró ninguna MiSTer. Prueba con --subnet 192.168.1.0/24 o revisa la red.");
        return Ok(());
    }

    println!("MiSTer candidatas encontradas ({}):", results.len());
    for host in &results {
        let status = if host.verified {
            "VERIFICADA"
        } else if host.ssh_banner.is_some() {
            "SSH"
        } else {
            "PUERTO ABIERTO"
        };
        println!(
            "  [{status}] {}:{} ({} ms)",
            host.ip, host.port, host.latency_ms
        );
        if let Some(banner) = &host.ssh_banner {
            println!("      banner: {banner}");
        }
        if let Some(note) = &host.verification_note {
            println!("      {note}");
        }
    }

    if let Some(best) = results.iter().find(|h| h.verified).or_else(|| results.first()) {
        println!("\nSugerencia:");
        println!(
            "  mister-cover-scraper list-remote --sftp {} --path /media/fat/games",
            best.ip
        );
    }

    Ok(())
}

/// Runs the `duplicates` CLI command: find same-name games and optionally delete chosen copies.
pub async fn run_duplicates(args: DuplicatesArgs) -> Result<()> {
    let is_sftp = args.sftp.is_some();
    let storage = if let Some(host) = args.sftp.clone() {
        println!("Conectando a {host}:{} por SFTP...", args.port);
        connect_sftp_client(
            host,
            args.port,
            args.user.clone(),
            args.password.clone(),
            args.key.clone(),
            args.passphrase.clone(),
            args.games.clone(),
        )
        .await?
    } else {
        StorageClient::Local
    };

    let cancel = AtomicBool::new(false);
    let games = storage
        .scan_games(&args.games, &cancel, |dir| println!("Escaneando: {dir}"))
        .await;
    let games = match games {
        Ok(games) => games,
        Err(err) => {
            storage.close().await;
            return Err(err);
        }
    };

    let groups = duplicates::find_duplicates(&games);
    println!("\nJuegos detectados: {} · grupos de duplicados: {}", games.len(), groups.len());

    if args.json && args.delete.is_empty() && args.keep.is_empty() {
        println!("{}", serde_json::to_string_pretty(&groups)?);
        storage.close().await;
        return Ok(());
    }

    if groups.is_empty() {
        println!("No se encontraron juegos duplicados.");
        storage.close().await;
        return Ok(());
    }

    for (group_idx, group) in groups.iter().enumerate() {
        println!("\n[{}] {}", group_idx + 1, group.key);
        for (game_idx, game) in group.games.iter().enumerate() {
            let tag = if game_idx == group.keep_idx {
                "KEEP"
            } else {
                "DUP "
            };
            println!(
                "   {tag} [{game_idx}] {} [{}]",
                game.path, game.extension
            );
        }
    }

    let mut to_delete = Vec::new();
    if !args.keep.is_empty() {
        for keep_path in &args.keep {
            let Some(group) = groups
                .iter()
                .find(|group| group.games.iter().any(|game| &game.path == keep_path))
            else {
                bail!("La ruta a conservar no pertenece a ningún grupo: {keep_path}");
            };
            let keep_idx = group
                .games
                .iter()
                .position(|game| &game.path == keep_path)
                .unwrap_or(group.keep_idx);
            to_delete.extend(duplicates::paths_to_delete(group, keep_idx));
        }
    } else if !args.delete.is_empty() {
        to_delete = duplicates::validate_delete_paths(&groups, &args.delete)?;
    } else {
        println!(
            "\nUsa --keep <RUTA>... para conservar una copia y borrar el resto, o --delete <RUTA>... para borrar rutas concretas."
        );
        storage.close().await;
        return Ok(());
    }

    let mut unique = Vec::new();
    for path in to_delete {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }

    println!("\nSe procesarán {} archivo(s):", unique.len());
    for path in &unique {
        println!("  - {path}");
    }
    println!(
        "{}",
        if args.dry_run {
            "DRY-RUN: no se eliminará nada."
        } else if is_sftp {
            "SFTP: los archivos se eliminarán permanentemente."
        } else {
            "Local: los archivos se moverán a una carpeta de papelera."
        }
    );

    if !args.dry_run && !args.yes {
        print!("¿Continuar? [s/N] ");
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let _ = io::stdin().read_line(&mut answer);
        let answer = answer.trim();
        if !answer.eq_ignore_ascii_case("s") && !answer.eq_ignore_ascii_case("si") {
            println!("Operación cancelada.");
            storage.close().await;
            return Ok(());
        }
    }

    let summary = duplicates::delete_paths(
        &storage,
        &args.games,
        is_sftp,
        &unique,
        args.dry_run,
        |message| println!("{message}"),
    )
    .await?;

    println!(
        "\nEliminados: {} · pistas de disco: {} · fallidos: {}",
        summary.deleted, summary.tracks_deleted, summary.failed
    );
    if let Some(ref trash) = summary.trash_dir {
        println!("Papelera: {trash}");
    }
    for (path, err) in &summary.errors {
        println!("  [ERROR] {path}: {err}");
    }

    storage.close().await;
    Ok(())
}

/// Runs the `sync` CLI command end-to-end.
pub async fn run_sync(args: SyncArgs) -> Result<()> {
    let storage = if let Some(host) = args.sftp {
        println!("Conectando a {host}:{} por SFTP...", args.port);
        connect_sftp_client(
            host,
            args.port,
            args.user,
            args.password,
            args.key,
            args.passphrase,
            args.games.clone(),
        )
        .await?
    } else {
        StorageClient::Local
    };

    let options = SyncOptions {
        threshold: args.threshold.clamp(0.0, 1.0),
        convert_to_png: !args.keep_format,
        overwrite_existing: args.overwrite,
        recursive_images: !args.no_recursive_images,
        media_placement: if args.per_subfolder_media {
            MediaPlacement::PerSubfolderMedia
        } else {
            MediaPlacement::RootMedia
        },
        dry_run: args.dry_run,
        strict_system_match: !args.allow_cross_system,
    };

    let cancel = AtomicBool::new(false);
    let analysis = sync::analyze(&storage, &args.games, &args.images, &options, &cancel, |msg| {
        println!("[INFO] {msg}");
    })
    .await;

    let analysis = match analysis {
        Ok(res) => res,
        Err(err) => {
            storage.close().await;
            return Err(err);
        }
    };

    let already_exists = analysis
        .items
        .iter()
        .filter(|i| i.status == MatchStatus::AlreadyExists)
        .count();
    let ready = analysis
        .items
        .iter()
        .filter(|i| i.selected_for_apply)
        .count();
    let unmatched = analysis
        .items
        .iter()
        .filter(|i| i.status == MatchStatus::Unmatched)
        .count();

    println!("\n=== Resumen del análisis ===");
    println!("Carpeta analizada : {}", analysis.games_root);
    println!("Juegos detectados : {}", analysis.items.len());
    println!("Covers indexados  : {}", analysis.covers.len());
    println!("Ya existentes     : {already_exists}");
    println!("Listos para copiar: {ready}");
    println!("Sin coincidencia  : {unmatched}");
    if let Some(ref warn) = analysis.system_mismatch_warning {
        println!("\n[ADVERTENCIA] {warn}");
    }

    if ready > 0 {
        println!(
            "\n{}",
            if options.dry_run {
                "--- Vista previa (DRY-RUN, no se escribirán archivos) ---"
            } else {
                "--- Emparejamientos a aplicar ---"
            }
        );
        for item in analysis.items.iter().filter(|i| i.selected_for_apply) {
            let cover_name = item
                .selected_cover_idx
                .and_then(|idx| analysis.covers.get(idx))
                .map(|c| c.file_name.as_str())
                .unwrap_or("?");
            let kind = item.match_kind.map(|k| k.label()).unwrap_or("-");
            let sys_tag = item
                .game
                .system
                .map(|s| format!("[{}] ", s.short_label()))
                .unwrap_or_default();
            println!(
                "  [{:.0}% | {}] {}{} -> {} (destino: {})",
                item.score * 100.0,
                kind,
                sys_tag,
                item.game.file_name,
                cover_name,
                item.target_full_path
            );
        }
    }

    if unmatched > 0 {
        println!("\n--- Juegos sin coincidencia por encima del umbral ---");
        for item in analysis
            .items
            .iter()
            .filter(|i| i.status == MatchStatus::Unmatched)
        {
            let suggestion = item
                .candidates
                .first()
                .and_then(|c| analysis.covers.get(c.cover_idx).map(|cov| (cov, c.score)));
            match suggestion {
                Some((cov, sc)) => println!(
                    "  [Mejor sugerencia {:.0}%] {} ~ {}",
                    sc * 100.0,
                    item.game.file_name,
                    cov.file_name
                ),
                None => println!("  [Sin sugerencias] {}", item.game.file_name),
            }
        }
    }

    let summary = sync::apply(
        &storage,
        &analysis.items,
        &analysis.covers,
        &options,
        &cancel,
        |evt| {
            if let ApplyEvent::ItemStarting {
                current,
                total,
                file_name,
            } = evt
            {
                let verb = if options.dry_run {
                    "Simulando"
                } else if storage.is_sftp() {
                    "Subiendo"
                } else {
                    "Copiando"
                };
                println!("[{current}/{total}] {verb}: {file_name}");
            }
        },
    )
    .await;

    storage.close().await;
    let summary = summary?;

    println!("\n=== Resultado final ===");
    println!("Total juegos      : {}", summary.total_games);
    println!("Ya tenían cover   : {}", summary.already_exists);
    println!(
        "{}: {}",
        if options.dry_run {
            "Se copiarían      "
        } else {
            "Copiados / subidos"
        },
        summary.copied
    );
    println!("Omitidos          : {}", summary.skipped);
    println!("Errores           : {}", summary.failed);
    for (game, err) in &summary.errors {
        eprintln!("  [ERROR] {game}: {err}");
    }
    println!(
        "\nConsejo ConsoleMode: Si acabas de añadir portadas, ejecuta 'Rebuild Game Database' y/o 'Optimize Artwork' en Game Library de ConsoleMode."
    );

    Ok(())
}

/// Runs the `upload` CLI command to route and upload games (files or `.cue`/`.bin` folders)
/// in bulk alongside matching covers.
pub async fn run_upload(args: UploadArgs) -> Result<()> {
    let default_system = args
        .system
        .as_deref()
        .map(|s| {
            SystemId::from_cli_str(s)
                .ok_or_else(|| anyhow::anyhow!("Consola desconocida en --system: {s}"))
        })
        .transpose()?;

    println!("Analizando orígenes para carga en lote...");
    let discovered = uploader::discover_upload_sources(&args.sources)?;
    if discovered.is_empty() {
        println!("No se encontraron juegos válidos en las rutas indicadas.");
        return Ok(());
    }

    let covers = if let Some(ref img_dir) = args.images {
        println!("Escaneando portadas locales en {}...", img_dir.display());
        scanner::scan_local_covers(img_dir, !args.no_recursive_images)?
    } else {
        Vec::new()
    };

    let storage = if let Some(host) = args.sftp {
        println!("Conectando a {host}:{} por SFTP...", args.port);
        connect_sftp_client(
            host,
            args.port,
            args.user,
            args.password,
            args.key,
            args.passphrase,
            args.games_root.clone(),
        )
        .await?
    } else {
        StorageClient::Local
    };

    let existing_dirs = uploader::resolve_existing_system_dirs(&storage, &args.games_root).await;
    let options = UploadOptions {
        default_system,
        overwrite_games: args.overwrite_games,
        overwrite_covers: args.overwrite_covers,
        convert_covers_to_png: !args.keep_format,
        strict_system_match: !args.allow_cross_system,
        cover_threshold: args.threshold.clamp(0.0, 1.0),
        wrap_disc_in_folder: args.wrap_disc_in_folder,
        media_placement: if args.per_subfolder_media {
            MediaPlacement::PerSubfolderMedia
        } else {
            MediaPlacement::RootMedia
        },
        convert_bin_cue_to_chd: args.convert_to_chd,
        chdman_path: args.chdman.clone(),
        temp_dir: args.temp_dir.clone(),
        dry_run: args.dry_run,
    };

    let items = uploader::build_upload_plan(
        discovered,
        &covers,
        &args.games_root,
        storage.is_sftp(),
        &existing_dirs,
        &options,
    );

    let ready_count = items
        .iter()
        .filter(|i| i.selected && i.target_system.is_some())
        .count();
    let needs_sys_count = items
        .iter()
        .filter(|i| i.status == UploadStatus::NeedsSystem)
        .count();
    let with_cover_count = items
        .iter()
        .filter(|i| i.selected && i.upload_cover && i.selected_cover_idx.is_some())
        .count();
    let chd_count = items
        .iter()
        .filter(|i| i.selected && i.convert_to_chd && i.payload.can_convert_to_chd())
        .count();
    let total_bytes = uploader::selected_total_bytes(&items);

    println!("\n=== Plan de carga en lote ===");
    println!("Destino MiSTer    : {}", args.games_root);
    println!("Juegos en cola    : {}", items.len());
    println!(
        "Listos para cargar: {ready_count} ({})",
        uploader::format_bytes(total_bytes)
    );
    println!("Conversión a CHD  : {chd_count}");
    println!("Con carátula      : {with_cover_count}");
    println!("Sin consola       : {needs_sys_count}");
    if covers.is_empty() && args.images.is_some() {
        println!("Aviso             : No se encontraron imágenes en --images.");
    }

    for item in &items {
        let mut kind_tag = match &item.payload {
            uploader::UploadPayload::SingleFile { .. } => "Archivo".to_string(),
            uploader::UploadPayload::AtomicFolder { files, .. } => {
                format!("Carpeta ({} archivos)", files.len())
            }
            uploader::UploadPayload::Archive {
                format,
                inner_files,
                is_disc_set: true,
                ..
            } => format!("{} ({} archivos)", format.label(), inner_files.len()),
            uploader::UploadPayload::Archive { format, .. } => {
                format!("{} (ROM)", format.label())
            }
        };
        if item.convert_to_chd && item.payload.can_convert_to_chd() {
            kind_tag.push_str(" -> CHD");
        }
        let sys_tag = item
            .target_system
            .map(|s| s.short_label())
            .unwrap_or("SIN CONSOLA");
        let cover_tag = item
            .selected_cover_idx
            .and_then(|idx| covers.get(idx))
            .map(|c| format!("{} ({:.0}%)", c.file_name, item.cover_score * 100.0))
            .unwrap_or_else(|| "Sin carátula".to_string());
        println!(
            "  [{sys_tag} | {kind_tag} | {}] {} -> {} | Cover: {cover_tag}",
            uploader::format_bytes(item.payload.total_bytes()),
            item.payload.display_name(),
            if item.target_game_path.is_empty() {
                "(usa --system para asignar consola)"
            } else {
                &item.target_game_path
            }
        );
    }

    if needs_sys_count > 0 && ready_count == 0 {
        println!(
            "\nNingún elemento tiene consola asignada. Indica --system (ej. --system PSX) para subir juegos con extensión ambigua."
        );
        storage.close().await;
        return Ok(());
    }

    let cancel = AtomicBool::new(false);
    let verb = if options.dry_run {
        "Simulando"
    } else if storage.is_sftp() {
        "Subiendo"
    } else {
        "Copiando"
    };

    let last_stage = Mutex::new(String::new());
    let summary = uploader::execute_upload(&storage, &items, &covers, &options, &cancel, |evt| {
        match evt {
            UploadEvent::ItemStarting {
                current,
                total,
                label,
                ..
            } => {
                if let Ok(mut guard) = last_stage.lock() {
                    guard.clear();
                }
                println!("[{current}/{total}] {verb}: {label}");
            }
            UploadEvent::StageProgress { stage, .. } => {
                if let Ok(mut guard) = last_stage.lock()
                    && *guard != stage
                {
                    *guard = stage.clone();
                    println!("  -> {stage}");
                }
            }
            UploadEvent::FileBytes { .. } => {}
            UploadEvent::ItemFinished { item_idx, status } => {
                if let UploadStatus::Failed(err) = status {
                    eprintln!(
                        "  [ERROR] {}: {err}",
                        items[item_idx].payload.display_name()
                    );
                }
            }
        }
    })
    .await;

    storage.close().await;
    let summary = summary?;

    println!("\n=== Resultado de la carga ===");
    println!("Total en cola     : {}", summary.total_items);
    println!("Juegos cargados   : {}", summary.games_uploaded);
    println!("Archivos extraídos: {}", summary.archives_extracted);
    println!("CHD convertidos   : {}", summary.chds_created);
    println!(
        "Archivos movidos  : {} ({})",
        summary.files_transferred,
        uploader::format_bytes(summary.bytes_transferred)
    );
    println!("Carátulas creadas : {}", summary.covers_uploaded);
    println!("Omitidos          : {}", summary.skipped);
    println!("Errores           : {}", summary.failed);
    for (name, err) in &summary.errors {
        eprintln!("  [ERROR] {name}: {err}");
    }
    println!(
        "\nConsejo ConsoleMode: Si subiste juegos y carátulas nuevos, ejecuta 'Rebuild Game Database' y/o 'Optimize Artwork' en ConsoleMode."
    );

    Ok(())
}
