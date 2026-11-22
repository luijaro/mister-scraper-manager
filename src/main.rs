mod archives;
mod chd;
mod cli;
mod config;
mod discover;
mod duplicates;
mod gui;
mod imaging;
mod matcher;
mod models;
mod scanner;
mod storage;
mod sync;
mod systems;
mod uploader;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "mister-cover-scraper",
    version,
    about = "Enlaza covers locales con juegos de MiSTer FPGA ConsoleMode (Local / SMB / SFTP)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Abre la interfaz gráfica de escritorio (por defecto si no se pasa subcomando)
    Gui,
    /// Escanea, empareja y copia/sube portadas faltantes a la carpeta media/
    Sync(cli::SyncArgs),
    /// Carga juegos o carpetas (incluidas carpetas con .bin/.cue) en lote y asigna sus carátulas
    Upload(cli::UploadArgs),
    /// Lista subcarpetas remotas en la MiSTer FPGA vía SFTP
    ListRemote(cli::ListRemoteArgs),
    /// Busca la IP de una MiSTer FPGA en la red local (SSH/SFTP)
    Discover(cli::DiscoverArgs),
    /// Detecta juegos con el mismo nombre y elimina los duplicados elegidos
    Duplicates(cli::DuplicatesArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None | Some(Commands::Gui) => gui::run_gui(),
        Some(Commands::Sync(args)) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(cli::run_sync(args))
        }
        Some(Commands::Upload(args)) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(cli::run_upload(args))
        }
        Some(Commands::ListRemote(args)) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(cli::run_list_remote(args))
        }
        Some(Commands::Discover(args)) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(cli::run_discover(args))
        }
        Some(Commands::Duplicates(args)) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(cli::run_duplicates(args))
        }
    }
}
