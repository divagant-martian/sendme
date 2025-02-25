//! Command line arguments.
use anyhow::Context;
use clap::{Parser, Subcommand};
use console::style;
use futures::{future, FutureExt, Stream, StreamExt};
use indicatif::{
    HumanBytes, HumanDuration, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle,
};
use iroh_bytes::{
    provider::{
        self, handle_connection, DownloadProgress, EventSender, RequestAuthorizationHandler,
    },
    store::{ExportMode, ImportMode, ImportProgress},
    BlobFormat, Hash, HashAndFormat, TempTag,
};
use iroh_bytes_util::get_hash_seq_and_sizes;
use iroh_net::{key::SecretKey, MagicEndpoint};
use rand::Rng;
use std::{
    collections::BTreeMap,
    fmt::{Display, Formatter},
    path::{Component, Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use walkdir::WalkDir;
mod sendme_ticket;
use sendme_ticket::Ticket;

use crate::collection::Collection;
mod collection;
mod get;
mod iroh_bytes_util;
mod progress;
/// Send a file or directory between two machines, using blake3 verified streaming.
///
/// For all subcommands, you can specify a secret key using the IROH_SECRET
/// environment variable. If you don't, a random one will be generated.
///
/// You can also specify a port for the magicsocket. If you don't, a random one
/// will be chosen.
#[derive(Parser, Debug)]
pub struct Args {
    #[clap(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    #[default]
    Hex,
    Cid,
}

impl FromStr for Format {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hex" => Ok(Format::Hex),
            "cid" => Ok(Format::Cid),
            _ => Err(anyhow::anyhow!("invalid format")),
        }
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Format::Hex => write!(f, "hex"),
            Format::Cid => write!(f, "cid"),
        }
    }
}

fn print_hash(hash: &Hash, format: Format) -> String {
    match format {
        Format::Hex => hash.to_hex().to_string(),
        Format::Cid => hash.to_string(),
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Provide a file or directory.
    Provide(ProvideArgs),

    /// Get a file or directory.
    Get(GetArgs),
}

#[derive(Parser, Debug)]
pub struct CommonArgs {
    /// The port for the magic socket to listen on.
    ///
    /// Defauls to a random free port, but it can be useful to specify a fixed
    /// port, e.g. to configure a firewall rule.
    #[clap(long, default_value_t = 0)]
    pub magic_port: u16,

    #[clap(long, default_value_t = Format::Hex)]
    pub format: Format,

    #[clap(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}
#[derive(Parser, Debug)]
pub struct ProvideArgs {
    /// Path to the file or directory to provide.
    ///
    /// The last component of the path will be used as the name of the data
    /// being shared.
    pub path: PathBuf,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Parser, Debug)]
pub struct GetArgs {
    /// The ticket to use to connect to the provider.
    pub ticket: sendme_ticket::Ticket,

    #[clap(flatten)]
    pub common: CommonArgs,
}

/// Get the secret key or generate a new one.
///
/// Print the secret key to stderr if it was generated, so the user can save it.
fn get_or_create_secret() -> anyhow::Result<SecretKey> {
    match std::env::var("IROH_SECRET") {
        Ok(secret) => SecretKey::from_str(&secret).context("invalid secret"),
        Err(_) => {
            let key = SecretKey::generate();
            eprintln!("using secret key {}", key);
            Ok(key)
        }
    }
}

#[derive(Debug)]
struct NoAuth;

impl RequestAuthorizationHandler for NoAuth {
    fn authorize(
        &self,
        _token: Option<iroh_bytes::protocol::RequestToken>,
        _request: &iroh_bytes::protocol::Request,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
        future::ok(()).boxed()
    }
}

fn validate_path_component(component: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !component.contains('/'),
        "path components must not contain the only correct path separator, /"
    );
    Ok(())
}

/// This function converts an already canonicalized path to a string.
///
/// If `must_be_relative` is true, the function will fail if any component of the path is
/// `Component::RootDir`
///
/// This function will also fail if the path is non canonical, i.e. contains
/// `..` or `.`, or if the path components contain any windows or unix path
/// separators.
pub fn canonicalized_path_to_string(
    path: impl AsRef<Path>,
    must_be_relative: bool,
) -> anyhow::Result<String> {
    let mut path_str = String::new();
    let parts = path
        .as_ref()
        .components()
        .filter_map(|c| match c {
            Component::Normal(x) => {
                let c = match x.to_str() {
                    Some(c) => c,
                    None => return Some(Err(anyhow::anyhow!("invalid character in path"))),
                };

                if !c.contains('/') && !c.contains('\\') {
                    Some(Ok(c))
                } else {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                }
            }
            Component::RootDir => {
                if must_be_relative {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                } else {
                    path_str.push('/');
                    None
                }
            }
            _ => Some(Err(anyhow::anyhow!("invalid path component {:?}", c))),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let parts = parts.join("/");
    path_str.push_str(&parts);
    Ok(path_str)
}

pub async fn show_ingest_progress(
    mut stream: impl Stream<Item = ImportProgress> + Unpin,
) -> anyhow::Result<()> {
    let mp = MultiProgress::new();
    mp.set_draw_target(ProgressDrawTarget::stderr());
    let op = mp.add(ProgressBar::hidden());
    op.set_style(
        ProgressStyle::default_spinner().template("{spinner:.green} [{elapsed_precise}] {msg}")?,
    );
    // op.set_message(format!("{} Ingesting ...\n", style("[1/2]").bold().dim()));
    // op.set_length(total_files);
    let mut names = BTreeMap::new();
    let mut sizes = BTreeMap::new();
    let mut pbs = BTreeMap::new();
    while let Some(event) = stream.next().await {
        match event {
            ImportProgress::Found { id, name } => {
                names.insert(id, name);
            }
            ImportProgress::Size { id, size } => {
                sizes.insert(id, size);
                let total_size = sizes.values().sum::<u64>();
                op.set_message(format!(
                    "{} Ingesting {} files, {}\n",
                    style("[1/2]").bold().dim(),
                    sizes.len(),
                    HumanBytes(total_size)
                ));
                let name = names.get(&id).cloned().unwrap_or_default();
                let pb = mp.add(ProgressBar::hidden());
                pb.set_style(ProgressStyle::with_template(
                    "{msg}{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}",
                )?.progress_chars("#>-"));
                pb.set_message(format!("{} {}", style("[2/2]").bold().dim(), name));
                pb.set_length(size);
                pbs.insert(id, pb);
            }
            ImportProgress::OutboardProgress { id, offset } => {
                if let Some(pb) = pbs.get(&id) {
                    pb.set_position(offset);
                }
            }
            ImportProgress::OutboardDone { id, .. } => {
                // you are not guaranteed to get any OutboardProgress
                if let Some(pb) = pbs.remove(&id) {
                    pb.finish_and_clear();
                }
            }
            ImportProgress::CopyProgress { .. } => {
                // we are not copying anything
            }
        }
    }
    op.finish_and_clear();
    Ok(())
}

/// Import from a file or directory into the database.
///
/// The returned tag always refers to a collection. If the input is a file, this
/// is a collection with a single blob, named like the file.
///
/// If the input is a directory, the collection contains all the files in the
/// directory.
async fn import(
    path: PathBuf,
    db: impl iroh_bytes::store::Store,
) -> anyhow::Result<(TempTag, u64, Collection)> {
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root = path.parent().context("context get parent")?;
    // walkdir also works for files, so we don't need to special case them
    let files = WalkDir::new(path.clone()).into_iter();
    // flatten the directory structure into a list of (name, path) pairs.
    // ignore symlinks.
    let data_sources: Vec<(String, PathBuf)> = files
        .map(|entry| {
            let entry = entry?;
            if !entry.file_type().is_file() {
                // Skip symlinks. Directories are handled by WalkDir.
                return Ok(None);
            }
            let path = entry.into_path();
            let relative = path.strip_prefix(root)?;
            let name = canonicalized_path_to_string(relative, true)?;
            anyhow::Ok(Some((name, path)))
        })
        .filter_map(Result::transpose)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let (send, recv) = flume::bounded(32);
    let progress = iroh_bytes::util::progress::FlumeProgressSender::new(send);
    let show_progress = tokio::spawn(show_ingest_progress(recv.into_stream()));
    // import all the files, using num_cpus workers, return names and temp tags
    let names_and_tags = futures::stream::iter(data_sources)
        .map(|(name, path)| {
            let db = db.clone();
            let progress = progress.clone();
            async move {
                let (temp_tag, file_size) = db
                    .import_file(path, ImportMode::TryReference, BlobFormat::Raw, progress)
                    .await?;
                anyhow::Ok((name, temp_tag, file_size))
            }
        })
        .buffer_unordered(num_cpus::get())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;
    drop(progress);
    // total size of all files
    let size = names_and_tags.iter().map(|(_, _, size)| *size).sum::<u64>();
    // collect the (name, hash) tuples into a collection
    // we must also keep the tags around so the data does not get gced.
    let (collection, tags) = names_and_tags
        .into_iter()
        .map(|(name, tag, _)| ((name, *tag.hash()), tag))
        .unzip::<_, _, Collection, Vec<_>>();
    let temp_tag = collection.clone().store(&db).await?;
    // now that the collection is stored, we can drop the tags
    // data is protected by the collection
    drop(tags);
    show_progress.await??;
    Ok((temp_tag, size, collection))
}

fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let parts = name.split('/');
    let mut path = root.to_path_buf();
    for part in parts {
        validate_path_component(part)?;
        path.push(part);
    }
    Ok(path)
}

async fn export(db: impl iroh_bytes::store::Store, root: HashAndFormat) -> anyhow::Result<()> {
    let collection = crate::collection::Collection::load(&db, &root.hash).await?;
    let root = std::env::current_dir()?;
    for (name, hash) in collection.iter() {
        let target = get_export_path(&root, name)?;
        db.export(*hash, target, ExportMode::TryReference, |_position| Ok(()))
            .await?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ProvideStatus {
    /// the multiprogress bar
    mp: MultiProgress,
}

impl ProvideStatus {
    fn new() -> Self {
        let mp = MultiProgress::new();
        mp.set_draw_target(ProgressDrawTarget::stderr());
        Self { mp }
    }

    fn new_client(&self) -> ClientStatus {
        let current = self.mp.add(ProgressBar::hidden());
        current.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {msg}")
                .unwrap(),
        );
        current.enable_steady_tick(Duration::from_millis(100));
        current.set_message("waiting for requests");
        ClientStatus {
            current: current.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct ClientStatus {
    current: Arc<ProgressBar>,
}

impl Drop for ClientStatus {
    fn drop(&mut self) {
        if Arc::strong_count(&self.current) == 1 {
            self.current.finish_and_clear();
        }
    }
}

impl EventSender for ClientStatus {
    fn send(&self, event: iroh_bytes::provider::Event) -> futures::prelude::future::BoxFuture<()> {
        tracing::info!("{:?}", event);
        let msg = match event {
            provider::Event::ClientConnected { connection_id } => {
                Some(format!("{} got connection", connection_id))
            }
            provider::Event::TransferBlobCompleted {
                connection_id,
                hash,
                index,
                size,
                ..
            } => Some(format!(
                "{} transfer blob completed {} {} {}",
                connection_id,
                hash,
                index,
                HumanBytes(size)
            )),
            provider::Event::TransferCompleted {
                connection_id,
                stats,
                ..
            } => Some(format!(
                "{} transfer completed {} {}",
                connection_id,
                stats.send.write_bytes.size,
                HumanDuration(stats.send.write_bytes.stats.duration)
            )),
            provider::Event::TransferAborted { connection_id, .. } => {
                Some(format!("{} transfer completed", connection_id))
            }
            _ => None,
        };
        if let Some(msg) = msg {
            self.current.set_message(msg);
        }
        future::ready(()).boxed()
    }
}

async fn provide(args: ProvideArgs) -> anyhow::Result<()> {
    let secret_key = get_or_create_secret()?;
    // create a magicsocket endpoint
    let endpoint_fut = MagicEndpoint::builder()
        .alpns(vec![iroh_bytes::protocol::ALPN.to_vec()])
        .secret_key(secret_key)
        .bind(args.common.magic_port);
    // use a flat store - todo: use a partial in mem store instead
    let suffix = rand::thread_rng().gen::<[u8; 16]>();
    let iroh_data_dir =
        std::env::current_dir()?.join(format!(".sendme-provide-{}", hex::encode(suffix)));
    if iroh_data_dir.exists() {
        println!("can not share twice from the same directory");
        std::process::exit(1);
    }
    let iroh_data_dir_2 = iroh_data_dir.clone();
    let _control_c = tokio::spawn(async move {
        tokio::signal::ctrl_c().await?;
        std::fs::remove_dir_all(iroh_data_dir_2)?;
        std::process::exit(1);
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });
    std::fs::create_dir_all(&iroh_data_dir)?;
    let rt = iroh_bytes::util::runtime::Handle::from_current(1)?;
    let db = iroh_bytes::store::flat::Store::load(
        iroh_data_dir.clone(),
        iroh_data_dir.clone(),
        iroh_data_dir.clone(),
        &rt,
    )
    .await?;
    let auth = Arc::new(NoAuth);
    let path = args.path;
    let (temp_tag, size, collection) = import(path.clone(), db.clone()).await?;
    let hash = *temp_tag.hash();
    // wait for the endpoint to be ready
    let endpoint = endpoint_fut.await?;
    // wait for the endpoint to figure out its address before making a ticket
    while endpoint.my_derp().is_none() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // make a ticket
    let addr = endpoint.my_addr().await?;
    let ticket = Ticket::new(addr, hash, BlobFormat::HashSeq)?;
    let entry_type = if path.is_file() { "file" } else { "directory" };
    println!(
        "imported {} {}, {}, hash {}",
        entry_type,
        path.display(),
        HumanBytes(size),
        print_hash(&hash, args.common.format)
    );
    if args.common.verbose > 0 {
        for (name, hash) in collection.iter() {
            println!("    {} {name}", print_hash(hash, args.common.format));
        }
    }
    println!("to get this data, use");
    println!("sendme get {}", ticket);
    let ps = ProvideStatus::new();
    loop {
        let Some(connecting) = endpoint.accept().await else {
            tracing::info!("no more incoming connections, exiting");
            break;
        };
        let db = db.clone();
        let rt = rt.clone();
        let ps = ps.clone();
        let auth = auth.clone();
        tokio::spawn(handle_connection(connecting, db, ps.new_client(), auth, rt));
    }
    drop(temp_tag);
    std::fs::remove_dir_all(iroh_data_dir)?;
    Ok(())
}

fn make_download_progress() -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb.set_style(
        ProgressStyle::with_template(
            "{msg}{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {binary_bytes_per_sec}",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    pb
}

pub async fn show_download_progress(
    mut stream: impl Stream<Item = DownloadProgress> + Unpin,
    total_size: u64,
) -> anyhow::Result<()> {
    let mp = MultiProgress::new();
    mp.set_draw_target(ProgressDrawTarget::stderr());
    let op = mp.add(make_download_progress());
    op.set_message(format!("{} Connecting ...\n", style("[1/3]").bold().dim()));
    let mut total_done = 0;
    let mut sizes = BTreeMap::new();
    while let Some(x) = stream.next().await {
        match x {
            DownloadProgress::Connected => {
                op.set_message(format!("{} Requesting ...\n", style("[2/3]").bold().dim()));
            }
            DownloadProgress::FoundHashSeq { children, .. } => {
                op.set_message(format!(
                    "{} Downloading {} blob(s)\n",
                    style("[3/3]").bold().dim(),
                    children + 1,
                ));
                op.set_length(total_size);
                op.reset();
            }
            DownloadProgress::Found { id, size, .. } => {
                sizes.insert(id, size);
            }
            DownloadProgress::Progress { offset, .. } => {
                op.set_position(total_done + offset);
            }
            DownloadProgress::Done { id } => {
                total_done += sizes.remove(&id).unwrap_or_default();
            }
            DownloadProgress::NetworkDone {
                bytes_read,
                elapsed,
                ..
            } => {
                op.finish_and_clear();
                eprintln!(
                    "Transferred {} in {}, {}/s",
                    HumanBytes(bytes_read),
                    HumanDuration(elapsed),
                    HumanBytes((bytes_read as f64 / elapsed.as_secs_f64()) as u64)
                );
            }
            DownloadProgress::AllDone => {
                break;
            }
            DownloadProgress::Abort(e) => {
                anyhow::bail!("download aborted: {:?}", e);
            }
            _ => {}
        }
    }
    Ok(())
}

async fn get(args: GetArgs) -> anyhow::Result<()> {
    let ticket = args.ticket;
    let addr = ticket.node_addr().clone();
    let secret_key = get_or_create_secret()?;
    let endpoint = MagicEndpoint::builder()
        .alpns(vec![])
        .secret_key(secret_key)
        .bind(args.common.magic_port)
        .await?;
    let dir_name = format!(".sendme-get-{}", ticket.hash().to_hex());
    let iroh_data_dir = std::env::current_dir()?.join(dir_name);
    let rt = iroh_bytes::util::runtime::Handle::from_current(1)?;
    let db = iroh_bytes::store::flat::Store::load(
        iroh_data_dir.clone(),
        iroh_data_dir.clone(),
        iroh_data_dir.clone(),
        &rt,
    )
    .await?;
    let mp = MultiProgress::new();
    let connect_progress = mp.add(ProgressBar::hidden());
    connect_progress.set_draw_target(ProgressDrawTarget::stderr());
    connect_progress.set_style(ProgressStyle::default_spinner());
    connect_progress.set_message(format!("connecting to {}", addr.node_id));
    let connection = endpoint.connect(addr, &iroh_bytes::protocol::ALPN).await?;
    let hash_and_format = HashAndFormat {
        hash: *ticket.hash(),
        format: ticket.format(),
    };
    connect_progress.finish_and_clear();
    let (send, recv) = flume::bounded(32);
    let progress = iroh_bytes::util::progress::FlumeProgressSender::new(send);
    let (_hash_seq, sizes) =
        get_hash_seq_and_sizes(&connection, &hash_and_format.hash, 1024 * 1024 * 32).await?;
    let total_size = sizes.iter().sum::<u64>();
    let total_files = sizes.len().saturating_sub(1);
    let payload_size = sizes.iter().skip(1).sum::<u64>();
    eprintln!("getting {} blobs, {}", sizes.len(), HumanBytes(total_size));
    eprintln!(
        "getting collection {} {} files, {}",
        print_hash(ticket.hash(), args.common.format),
        total_files,
        HumanBytes(payload_size)
    );
    let _task = tokio::spawn(show_download_progress(recv.into_stream(), total_size));
    let _stats = get::get(&db, connection, &hash_and_format, progress).await?;
    if args.common.verbose > 0 {
        let collection = Collection::load(&db, &hash_and_format.hash).await?;
        for (name, hash) in collection.iter() {
            println!("    {} {name}", print_hash(hash, args.common.format));
        }
    }
    export(db, hash_and_format).await?;
    std::fs::remove_dir_all(iroh_data_dir)?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let res = match args.command {
        Commands::Provide(args) => provide(args).await,
        Commands::Get(args) => get(args).await,
    };
    match res {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1)
        }
    }
}
