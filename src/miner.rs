use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use num_cpus;

use crate::{Block, Bytes, Context, Keystore, setup_miner_thread};
use crate::commons::{CHAIN_VERSION, LOCKER_DIFFICULTY, KEYSTORE_DIFFICULTY};
use crate::blockchain::types::BlockQuality;
use crate::blockchain::hash_utils::*;
use crate::keys::check_public_key_strength;
use crate::event::Event;
use blakeout::Blakeout;
use std::ops::Deref;

pub struct Miner {
    context: Arc<Mutex<Context>>,
    jobs: Arc<Mutex<Vec<MineJob>>>,
    running: Arc<AtomicBool>,
    mining: Arc<AtomicBool>,
    cond_var: Arc<Condvar>
}

impl Miner {
    pub fn new(context: Arc<Mutex<Context>>) -> Self {
        Miner {
            context,
            jobs: Arc::new(Mutex::new(Vec::new())),
            running: Arc::new(AtomicBool::new(false)),
            mining: Arc::new(AtomicBool::new(false)),
            cond_var: Arc::new(Condvar::new())
        }
    }

    pub fn add_block(&mut self, block: Block, keystore: Keystore) {
        self.jobs.lock().unwrap().push(MineJob { block, keystore });
        self.cond_var.notify_one();
    }

    pub fn stop(&mut self) {
        self.mining.store(false, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        self.cond_var.notify_all();
    }

    pub fn start_mining_thread(&mut self) {
        let context = Arc::clone(&self.context);
        let blocks = self.jobs.clone();
        let running = self.running.clone();
        let mining = self.mining.clone();
        let cond_var = self.cond_var.clone();
        thread::spawn(move || {
            running.store(true, Ordering::SeqCst);
            while running.load(Ordering::SeqCst) {
                // If some transaction is being mined now, we yield
                if mining.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1000));
                    continue;
                }

                let mut lock = blocks.lock().unwrap();
                if lock.len() > 0 {
                    info!("Got new block to mine");
                    let block = lock.remove(0);
                    mining.store(true, Ordering::SeqCst);
                    Miner::mine_internal(Arc::clone(&context), block, mining.clone());
                } else {
                    let _ = cond_var.wait(lock).expect("Error in wait lock!");
                }
            }
        });
        let mining = self.mining.clone();
        let blocks = self.jobs.clone();
        let cond_var = self.cond_var.clone();
        self.context.lock().unwrap().bus.register(move |_uuid, e| {
            match e {
                Event::NewBlockReceived => {}
                Event::BlockchainChanged {..} => {}
                Event::ActionStopMining => {
                    mining.store(false, Ordering::SeqCst);
                }
                Event::ActionMineLocker { index, hash, keystore } => {
                    if !mining.load(Ordering::SeqCst) {
                        let mut block = Block::new(None, Bytes::default(), hash, LOCKER_DIFFICULTY);
                        block.index = index;
                        blocks.lock().unwrap().push(MineJob { block, keystore: keystore.deref().clone() });
                        cond_var.notify_all();
                        info!("Added a locker block to mine");
                    }
                }
                _ => {}
            }
            true
        });
    }

    pub fn is_mining(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    fn mine_internal(context: Arc<Mutex<Context>>, mut job: MineJob, mining: Arc<AtomicBool>) {
        // Clear signature and hash just in case
        job.block.signature = Bytes::default();
        job.block.hash = Bytes::default();
        job.block.version = CHAIN_VERSION;
        // If this block needs to be a locker
        if job.block.index > 0 && !job.block.prev_block_hash.is_empty() {
            info!("Mining locker block");
            job.block.pub_key = job.keystore.get_public();
            if !check_public_key_strength(&job.block.pub_key, KEYSTORE_DIFFICULTY) {
                warn!("Can not mine block with weak public key!");
                context.lock().unwrap().bus.post(Event::MinerStopped { success: false, full: false });
                mining.store(false, Ordering::SeqCst);
                return;
            }
            match context.lock().unwrap().chain.last_block() {
                None => {}
                Some(last_block) => {
                    debug!("Last block found");
                    // If we were doing something else and got new block before we could mine this block
                    if last_block.index > job.block.index || last_block.hash != job.block.prev_block_hash {
                        warn!("We missed block to lock");
                        context.lock().unwrap().bus.post(Event::MinerStopped { success: false, full: false });
                        mining.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
        } else {
            job.block.index = context.lock().unwrap().chain.height() + 1;
            job.block.prev_block_hash = match context.lock().unwrap().chain.last_block() {
                None => { Bytes::default() }
                Some(block) => { block.hash }
            };
        }

        context.lock().unwrap().bus.post(Event::MinerStarted);
        let thread_spawn_interval = Duration::from_millis(10);
        let live_threads = Arc::new(AtomicU32::new(0u32));
        let lower = context.lock().unwrap().settings.mining.lower;
        let cpus = num_cpus::get();
        let threads = context.lock().unwrap().settings.mining.threads;
        let threads = match threads {
            0 => cpus,
            _ => threads
        };
        debug!("Starting {} threads for mining", threads);
        for cpu in 0..threads {
            let context = Arc::clone(&context);
            let job = job.clone();
            let mining = Arc::clone(&mining);
            let live_threads = Arc::clone(&live_threads);
            thread::spawn(move || {
                live_threads.fetch_add(1, Ordering::SeqCst);
                if lower {
                    setup_miner_thread(cpu as u32);
                }
                let full = job.block.transaction.is_some();
                match find_hash(Arc::clone(&context), job.block, Arc::clone(&mining), cpu) {
                    None => {
                        debug!("Mining was cancelled");
                        let count = live_threads.fetch_sub(1, Ordering::SeqCst);
                        // If this is the last thread, but mining was not stopped by another thread
                        if count == 1 {
                            let mut context = context.lock().unwrap();
                            context.bus.post(Event::MinerStopped { success: false, full });
                        }
                    },
                    Some(mut block) => {
                        let index = block.index;
                        let mut context = context.lock().unwrap();
                        block.signature = Bytes::from_bytes(&job.keystore.sign(&block.as_bytes()));
                        let mut success = false;
                        if context.chain.check_new_block(&block) != BlockQuality::Good {
                            warn!("Error adding mined block!");
                            if index == 0 {
                                error!("To mine genesis block you need to make 'origin' an empty string in config.");
                            }
                        } else {
                            if block.index == 1 {
                                context.settings.origin = block.hash.to_string();
                            }
                            context.chain.add_block(block);
                            success = true;
                        }
                        context.bus.post(Event::MinerStopped { success, full });
                        mining.store(false, Ordering::SeqCst);
                    },
                }
            });
            thread::sleep(thread_spawn_interval);
        }
    }
}

#[derive(Clone)]
pub struct MineJob {
    block: Block,
    keystore: Keystore
}

fn find_hash(context: Arc<Mutex<Context>>, mut block: Block, running: Arc<AtomicBool>, thread: usize) -> Option<Block> {
    let difficulty = block.difficulty;
    let full = block.transaction.is_some();
    let mut digest = Blakeout::new();
    let mut max_diff = 0;
    loop {
        block.random = rand::random();
        block.timestamp = Utc::now().timestamp();
        let next_allowed_block = {
            let context = context.lock().unwrap();
            // We use this block to fill some fields of our block as well
            block.index = context.chain.height() + 1;
            if let Some(b) = context.chain.last_block() {
                block.prev_block_hash = b.hash;
            }
            context.chain.next_allowed_block()
        };

        if full && next_allowed_block > block.index {
            // We can't mine now, as we need to wait for block to be signed
            thread::sleep(Duration::from_millis(1000));
            continue;
        }
        debug!("Mining block {}", serde_json::to_string(&block).unwrap());
        let mut time = Instant::now();
        let mut prev_nonce = 0;
        for nonce in 0..std::u64::MAX {
            if !running.load(Ordering::Relaxed) {
                return None;
            }
            block.nonce = nonce;

            digest.reset();
            digest.update(&block.as_bytes());
            let diff = hash_difficulty(digest.result());
            if diff >= difficulty {
                block.hash = Bytes::from_bytes(digest.result());
                return Some(block);
            }
            if diff > max_diff {
                max_diff = diff;
            }

            let elapsed = time.elapsed().as_millis();
            if elapsed >= 1000 {
                block.timestamp = Utc::now().timestamp();
                if elapsed > 5000 {
                    let speed = (nonce - prev_nonce) / (elapsed as u64 / 1000);
                    //debug!("Mining speed {} H/s, max difficulty {}", speed, max_diff);
                    if let Ok(mut context) = context.lock() {
                        context.bus.post(Event::MinerStats { thread, speed, max_diff})
                    }
                    time = Instant::now();
                    prev_nonce = nonce;
                }

                if block.index > 1 {
                    if let Ok(context) = context.lock() {
                        if context.chain.height() >= block.index {
                            break;
                        }
                    }
                }
            }
        }
    }
}