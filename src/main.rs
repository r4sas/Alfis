// With the default subsystem, 'console', windows creates an additional console window for the program.
// This is silently ignored on non-windows systems.
// See https://msdn.microsoft.com/en-us/library/4cc7ya5b.aspx for more details.
#![windows_subsystem = "windows"]
extern crate web_view;
extern crate tinyfiledialogs as tfd;
extern crate serde;
extern crate serde_json;

use std::env;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use winapi::um::wincon::{AttachConsole, FreeConsole, ATTACH_PARENT_PROCESS};

use rand::RngCore;
use thread_priority::*;
use serde::Deserialize;
use web_view::*;
use getopts::Options;
use simple_logger::{SimpleLogger};
#[allow(unused_imports)]
use log::{trace, debug, info, warn, error, LevelFilter};

use alfis::{Blockchain, Bytes, Context, Keystore, Transaction, check_domain, Block};
use alfis::event::Event;
use alfis::miner::Miner;
use alfis::p2p::Network;
use alfis::settings::Settings;
use alfis::dns::context::{ServerContext, ResolveStrategy};
use alfis::dns::server::{DnsServer, DnsUdpServer, DnsTcpServer};
use alfis::dns::protocol::DnsRecord;
use alfis::blockchain::filter::BlockchainFilter;

const KEYSTORE_DIFFICULTY: usize = 24;
const SETTINGS_FILENAME: &str = "alfis.toml";
const LOG_TARGET_MAIN: &str = "alfis::Main";
const LOG_TARGET_UI: &str = "alfis::UI";

fn main() {
    // When linked with the windows subsystem windows won't automatically attach
    // to the console of the parent process, so we do it explicitly. This fails silently if the parent has no console.
    #[cfg(windows)]
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }

    let args: Vec<String> = env::args().collect();
    let program = args[0].clone();

    let mut opts = Options::new();
    opts.optflag("h", "help", "Print this help menu");
    opts.optflag("n", "nogui", "Run without graphic user interface");
    opts.optflag("v", "verbose", "Show more debug messages");
    opts.optflag("d", "debug", "Show trace messages, more than debug");
    opts.optflag("l", "list", "List blocks from DB and exit");
    opts.optopt("c", "config", "Path to config file", "");

    let opt_matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => panic!(f.to_string()),
    };

    if opt_matches.opt_present("h") {
        let brief = format!("Usage: {} [options]", program);
        print!("{}", opts.usage(&brief));
        return;
    }

    let no_gui = opt_matches.opt_present("n");

    let mut level = LevelFilter::Info;
    if opt_matches.opt_present("v") {
        level = LevelFilter::Debug;
    }
    if opt_matches.opt_present("d") {
        level = LevelFilter::Trace;
    }
    let config_name = match opt_matches.opt_str("c") {
        None => { SETTINGS_FILENAME.to_owned() }
        Some(path) => { path }
    };
    SimpleLogger::new().with_level(level).init().unwrap();
    info!(target: LOG_TARGET_MAIN, "Starting ALFIS {}", env!("CARGO_PKG_VERSION"));

    let settings = Settings::load(&config_name).expect("Error loading settings");
    let keystore: Keystore = match Keystore::from_file(&settings.key_file, "") {
        None => {
            warn!(target: LOG_TARGET_MAIN, "Generated temporary keystore. Please, generate full-privileged keys.");
            Keystore::new()
        }
        Some(keystore) => { keystore }
    };
    let blockchain: Blockchain = Blockchain::new(&settings);
    if opt_matches.opt_present("l") {
        for i in 1..(blockchain.height() + 1) {
            if let Some(block) = blockchain.get_block(i) {
                info!(target: LOG_TARGET_MAIN, "{:?}", &block);
            }
        }
        return;
    }

    match blockchain.get_block(1) {
        None => { info!(target: LOG_TARGET_MAIN, "No blocks found in DB"); }
        Some(block) => { trace!(target: LOG_TARGET_MAIN, "Loaded DB with origin {:?}", &block.hash); }
    }
    let settings_copy = settings.clone();
    let context: Arc<Mutex<Context>> = Arc::new(Mutex::new(Context::new(settings, keystore, blockchain)));
    start_dns_server(&context, &settings_copy);

    let mut miner_obj = Miner::new(context.clone());
    miner_obj.start_mining_thread();
    let miner: Arc<Mutex<Miner>> = Arc::new(Mutex::new(miner_obj));

    let mut network = Network::new(context.clone());
    network.start().expect("Error starting network component");

    create_genesis_if_needed(&context, &miner);
    if no_gui {
        let sleep = Duration::from_millis(1000);
        loop {
            thread::sleep(sleep);
        }
    } else {
        run_interface(context.clone(), miner.clone());
    }

    // Without explicitly detaching the console cmd won't redraw it's prompt.
    #[cfg(windows)]
    unsafe {
        FreeConsole();
    }
}

fn start_dns_server(context: &Arc<Mutex<Context>>, settings: &Settings) {
    let server_context = create_server_context(context.clone(), &settings);

    if server_context.enable_udp {
        let udp_server = DnsUdpServer::new(server_context.clone(), 20);
        if let Err(e) = udp_server.run_server() {
            error!(target: LOG_TARGET_MAIN, "Failed to bind UDP listener: {:?}", e);
        }
    }

    if server_context.enable_tcp {
        let tcp_server = DnsTcpServer::new(server_context.clone(), 20);
        if let Err(e) = tcp_server.run_server() {
            error!(target: LOG_TARGET_MAIN, "Failed to bind TCP listener: {:?}", e);
        }
    }
}

fn create_genesis_if_needed(context: &Arc<Mutex<Context>>, miner: &Arc<Mutex<Miner>>) {
    // If there is no origin in settings and no blockchain in DB, generate genesis block
    let context = context.lock().unwrap();
    let last_block = context.get_blockchain().last_block();
    let origin = context.settings.origin.clone();
    if origin.eq("") && last_block.is_none() {
        // If blockchain is empty, we are going to mine a Genesis block
        create_genesis(miner.clone(), &context.get_keystore());
    }
}

fn run_interface(context: Arc<Mutex<Context>>, miner: Arc<Mutex<Miner>>) {
    let file_content = include_str!("webview/index.html");
    let mut styles = inline_style(include_str!("webview/bulma.css"));
    styles.push_str(&inline_style(include_str!("webview/busy_indicator.css")));
    let scripts = inline_script(include_str!("webview/scripts.js"));

    let html = Content::Html(file_content.to_owned().replace("{styles}", &styles).replace("{scripts}", &scripts));
    let title = format!("ALFIS {}", env!("CARGO_PKG_VERSION"));
    let mut interface = web_view::builder()
        .title(&title)
        .content(html)
        .size(1023, 720)
        .min_size(895, 350)
        .resizable(true)
        .debug(false)
        .user_data(())
        .invoke_handler(|web_view, arg| {
            use Cmd::*;
            debug!(target: LOG_TARGET_UI, "Command {}", arg);
            match serde_json::from_str(arg).unwrap() {
                Loaded => {
                    web_view.eval("showMiningIndicator(false, false);").expect("Error evaluating!");
                    let handle = web_view.handle();
                    let mut status = Status::new();
                    let mut c = context.lock().unwrap();
                    c.bus.register(move |_uuid, e| {
                        debug!(target: LOG_TARGET_UI, "Got event from bus {:?}", &e);
                        let eval = match e {
                            Event::KeyCreated { path, public } => { format!("keystoreChanged('{}', '{}');", &path, &public) }
                            Event::KeyLoaded { path, public } => { format!("keystoreChanged('{}', '{}');", &path, &public) }
                            Event::KeySaved { path, public } => { format!("keystoreChanged('{}', '{}');", &path, &public) }
                            Event::MinerStarted => {
                                status.mining = true;
                                String::from("setLeftStatusBarText('Mining...'); showMiningIndicator(true, false);")
                            }
                            Event::KeyGeneratorStarted => {
                                status.mining = true;
                                String::from("setLeftStatusBarText('Mining...'); showMiningIndicator(true, false);")
                            }
                            Event::MinerStopped | Event::KeyGeneratorStopped => {
                                status.mining = false;
                                if status.syncing {
                                    String::from("setLeftStatusBarText('Syncing...'); showMiningIndicator(true, true);")
                                } else {
                                    String::from("setLeftStatusBarText('Idle'); showMiningIndicator(false, false);")
                                }
                            }
                            Event::Syncing { have, height } => {
                                status.syncing = true;
                                status.synced_blocks = have;
                                status.sync_height = height;
                                if status.mining {
                                    String::from("setLeftStatusBarText('Mining...'); showMiningIndicator(true, false);")
                                } else {
                                    format!("setLeftStatusBarText('Synchronizing {}/{}'); showMiningIndicator(true, true);", have, height)
                                }
                            }
                            Event::SyncFinished => {
                                status.syncing = false;
                                if status.mining {
                                    String::from("setLeftStatusBarText('Mining...'); showMiningIndicator(true, false);")
                                } else {
                                    format!("setLeftStatusBarText('Idle'); showMiningIndicator(false, false);")
                                }
                            }
                            Event::NetworkStatus { nodes, blocks } => {
                                if status.mining || status.syncing || nodes < 3 {
                                    format!("setRightStatusBarText('Nodes: {}, Blocks: {}')", nodes, blocks)
                                } else {
                                    format!("setLeftStatusBarText('Idle'); setRightStatusBarText('Nodes: {}, Blocks: {}')", nodes, blocks)
                                }
                            }
                            _ => { String::new() }
                        };

                        if !eval.is_empty() {
                            debug!(target: LOG_TARGET_UI, "Evaluating {}", &eval);
                            handle.dispatch(move |web_view| {
                                web_view.eval(&eval.replace("\\", "\\\\"))
                            }).expect("Error dispatching!");
                        }
                        true
                    });
                    let eval = format!("keystoreChanged('{}', '{}');", c.keystore.get_path(), &c.keystore.get_public().to_string());
                    debug!(target: LOG_TARGET_UI, "Evaluating {}", &eval);
                    web_view.eval(&eval.replace("\\", "\\\\")).expect("Error evaluating!");
                }
                LoadKey {} => {
                    let result = tfd::open_file_dialog("Open keys file", "", Some((&["*.key"], "*.key")));
                    match result {
                        None => {}
                        Some(file_name) => {
                            match Keystore::from_file(&file_name, "") {
                                None => {
                                    error!(target: LOG_TARGET_UI, "Error loading keystore '{}'!", &file_name);
                                }
                                Some(keystore) => {
                                    info!(target: LOG_TARGET_UI, "Loaded keystore with key: {:?}", &keystore.get_public());
                                    let mut c = context.lock().unwrap();
                                    c.bus.post(Event::KeyLoaded { path: keystore.get_path().to_owned(), public: keystore.get_public().to_string() });
                                    c.set_keystore(keystore);
                                }
                            }
                        }
                    }
                }
                CreateKey {} => {
                    create_key(context.clone());
                }
                SaveKey {} => {
                    let result = tfd::save_file_dialog_with_filter("Save keys file", "", &["*.key"], "Key files (*.key)");
                    match result {
                        None => {}
                        Some(new_path) => {
                            let mut c = context.lock().unwrap();
                            let path = new_path.clone();
                            let public = c.keystore.get_public().to_string();
                            c.keystore.save(&new_path, "");
                            info!(target: LOG_TARGET_UI, "Key file saved to {}", &path);
                            c.bus.post(Event::KeySaved { path, public });
                        }
                    }
                }
                CheckDomain { name } => {
                    let name = name.to_lowercase();
                    let c = context.lock().unwrap();
                    let available = c.get_blockchain().is_domain_available(&name, &c.get_keystore());
                    web_view.eval(&format!("domainAvailable({})", available)).expect("Error evaluating!");
                }
                CreateDomain { name, records, .. } => {
                    debug!(target: LOG_TARGET_UI, "Got records: {}", records);
                    let name = name.to_lowercase();
                    if !check_domain(&name, true) {
                        return Ok(());
                    }
                    if serde_json::from_str::<Vec<DnsRecord>>(&records).is_ok() {
                        let (keystore, transaction) = {
                            let context = context.lock().unwrap();
                            (context.get_keystore(), context.blockchain.get_domain_transaction(&name))
                        };
                        match transaction {
                            None => {
                                create_domain(miner.clone(), name, records, &keystore);
                            }
                            Some(transaction) => {
                                if transaction.pub_key == keystore.get_public() {
                                    create_domain(miner.clone(), name, records, &keystore);
                                } else {
                                    warn!(target: LOG_TARGET_UI, "Tried to mine not owned domain!");
                                    let _ = web_view.eval(&format!("showWarning('{}');", "You cannot change domain that you don't own!"));
                                }
                            }
                        }
                    } else {
                        warn!(target: LOG_TARGET_UI, "Error in DNS records for domain!");
                        let _ = web_view.eval(&format!("showWarning('{}');", "Something wrong with your records! Please, correct the error and try again."));
                    }
                }
                ChangeDomain { .. } => {}
                RenewDomain { .. } => {}
                TransferDomain { .. } => {}
                CheckZone { name } => {
                    let name = name.to_lowercase();
                    if !check_domain(&name, false) {
                        web_view.eval("zoneAvailable(false)").expect("Error evaluating!");
                    } else {
                        let c = context.lock().unwrap();
                        let available = c.get_blockchain().is_domain_available(&name, &c.get_keystore());
                        web_view.eval(&format!("zoneAvailable({})", available)).expect("Error evaluating!");
                    }
                }
                CreateZone { name, data } => {
                    let name = name.to_lowercase();
                    let data = data.to_lowercase();
                    let (keystore, transaction) = {
                        let context = context.lock().unwrap();
                        (context.get_keystore(), context.blockchain.get_domain_transaction(&name))
                    };
                    match transaction {
                        None => {
                            create_domain(miner.clone(), name, data, &keystore);
                        }
                        Some(transaction) => {
                            if transaction.pub_key == keystore.get_public() {
                                create_domain(miner.clone(), name, data, &keystore);
                            } else {
                                warn!(target: LOG_TARGET_UI, "Tried to mine not owned domain!");
                                let _ = web_view.eval(&format!("showWarning('{}');", "You cannot change domain that you don't own!"));
                            }
                        }
                    }
                }
                StopMining => {
                    context.lock().unwrap().bus.post(Event::ActionStopMining);
                }
            }
            //dbg!(&signature);
            Ok(())
        })
        .build()
        .expect("Error building GUI");

    // We use this ugly loop to lower CPU usage a lot.
    // If we use .run() or only .step() in a loop without sleeps it will try
    // to support 60FPS and uses more CPU than it should.
    let pause = Duration::from_millis(25);
    let mut start = Instant::now();
    loop {
        match interface.step() {
            None => {
                info!(target: LOG_TARGET_UI, "Interface closed, exiting");
                break;
            }
            Some(result) => {
                match result {
                    Ok(_) => {}
                    Err(_) => {
                        error!(target: LOG_TARGET_UI, "Something wrong with webview, exiting");
                        break;
                    }
                }
            }
        }
        if start.elapsed().as_millis() > 1 {
            thread::sleep(pause);
            start = Instant::now();
        }
    }
    interface.exit();
}

fn create_genesis(miner: Arc<Mutex<Miner>>, keystore: &Keystore) {
    let block = Block::new(None, keystore.get_public(), Bytes::default());
    let mut miner_guard = miner.lock().unwrap();
    miner_guard.add_block(block);
}

fn create_domain<S: Into<String>>(miner: Arc<Mutex<Miner>>, name: S, data: S, keystore: &Keystore) {
    let name = name.into();
    info!(target: LOG_TARGET_UI, "Generating domain or zone {}", name);
    //let tags_vector: Vec<String> = tags.into().trim().split(",").map(|s| s.trim()).map(String::from).collect();
    let transaction = Transaction::from_str(name.into(), "dns".into(), data.into(), keystore.get_public().clone());
    let block = Block::new(Some(transaction), keystore.get_public(), Bytes::default());
    let mut miner_guard = miner.lock().unwrap();
    miner_guard.add_block(block);
}

fn create_key(context: Arc<Mutex<Context>>) {
    let mining = Arc::new(AtomicBool::new(true));
    let miners_count = Arc::new(AtomicUsize::new(0));
    { context.lock().unwrap().bus.post(Event::KeyGeneratorStarted); }
    for _ in 0..num_cpus::get() {
        let context = context.clone();
        let mining = mining.clone();
        let miners_count = miners_count.clone();
        thread::spawn(move || {
            let _ = set_current_thread_priority(ThreadPriority::Min);
            miners_count.fetch_add(1, Ordering::Relaxed);
            match generate_key(KEYSTORE_DIFFICULTY, mining.clone()) {
                None => {
                    debug!(target: LOG_TARGET_UI, "Keystore mining finished");
                }
                Some(keystore) => {
                    info!(target: LOG_TARGET_UI, "Key mined successfully: {:?}", &keystore.get_public());
                    let mut c = context.lock().unwrap();
                    mining.store(false, Ordering::Relaxed);
                    c.bus.post(Event::KeyCreated { path: keystore.get_path().to_owned(), public: keystore.get_public().to_string() });
                    c.set_keystore(keystore);
                }
            }
            let miners = miners_count.fetch_sub(1, Ordering::Relaxed) - 1;
            if miners == 0 {
                context.lock().unwrap().bus.post(Event::KeyGeneratorStopped);
            }
        });
    }
    context.lock().unwrap().bus.register(move |_uuid, e| {
        if e == Event::ActionStopMining {
            mining.store(false, Ordering::Relaxed);
        }
        false
    });
}

fn generate_key(difficulty: usize, mining: Arc<AtomicBool>) -> Option<Keystore> {
    let mut rng = rand::thread_rng();
    let mut buf = [0u8; 32];
    loop {
        rng.fill_bytes(&mut buf);
        let keystore = Keystore::from_bytes(&buf);
        if keystore.hash_is_good(difficulty) {
            info!(target: LOG_TARGET_UI, "Generated keypair: {:?}", &keystore);
            return Some(keystore);
        }
        if !mining.load(Ordering::Relaxed) {
            return None;
        }
    }
}

fn create_server_context(context: Arc<Mutex<Context>>, settings: &Settings) -> Arc<ServerContext> {
    let mut server_context = ServerContext::new();
    server_context.allow_recursive = true;
    server_context.dns_host = settings.dns.host.clone();
    server_context.dns_port = settings.dns.port;
    server_context.resolve_strategy = match settings.dns.forwarders.is_empty() {
        true => { ResolveStrategy::Recursive }
        false => { ResolveStrategy::Forward { upstreams: settings.dns.forwarders.clone() } }
    };
    server_context.filters.push(Box::new(BlockchainFilter::new(context)));
    match server_context.initialize() {
        Ok(_) => {}
        Err(e) => { panic!("DNS server failed to initialize: {:?}", e); }
    }

    Arc::new(server_context)
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum Cmd {
    Loaded,
    LoadKey {},
    CreateKey {},
    SaveKey {},
    CheckZone { name: String },
    CreateZone { name: String, data: String },
    CheckDomain { name: String },
    CreateDomain { name: String, records: String, tags: String },
    ChangeDomain { name: String, records: String, tags: String },
    RenewDomain { name: String, days: u16 },
    TransferDomain { name: String, owner: String },
    StopMining,
}

struct Status {
    pub mining: bool,
    pub syncing: bool,
    pub synced_blocks: u64,
    pub sync_height: u64,
    pub nodes_connected: usize,
    pub chain_height: u64
}

impl Status {
    fn new() -> Self {
        Status { mining: false, syncing: false, synced_blocks: 0, sync_height: 0, nodes_connected: 0, chain_height: 0 }
    }
}

fn inline_style(s: &str) -> String {
    format!(r#"<style type="text/css">{}</style>"#, s)
}

fn inline_script(s: &str) -> String {
    format!(r#"<script type="text/javascript">{}</script>"#, s)
}

#[cfg(test)]
mod tests {
    use alfis::dns::protocol::{DnsRecord, TransientTtl};

    #[test]
    fn record_to_string() {
        let record = DnsRecord::A {
            domain: "google.com".to_string(),
            addr: "127.0.0.1".parse().unwrap(),
            ttl: TransientTtl(300)
        };
        println!("Record is {:?}", &record);
        println!("Record in JSON is {}", serde_json::to_string(&record).unwrap());
    }
}