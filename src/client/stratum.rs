use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use futures::prelude::*;
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

mod statum_codec;

use statum_codec::NewLineJsonCodec;
use crate::{miner::MinerManager, Error, Uint256};
use log::{error, info, warn};
use rand::{RngCore, thread_rng};
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::{ReceiverStream};
use crate::client::Client;
use async_trait::async_trait;
use crate::client::stratum::statum_codec::{ErrorCode, NewLineJsonCodecError, StratumLine, MiningNotify};
use futures_util::TryStreamExt;
use num::Float;
use parking::Mutex;
use tokio::task;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::PollSender;
use crate::client::stratum::statum_codec::StratumCommand;
use crate::pow::BlockSeed;
use crate::pow::BlockSeed::PartialBlock;

//const DIFFICULTY_1_TARGET: Uint256 = Uint256([0x00000000ffff0000, 0x0000000000000000, 0x0000000000000000, 0x0000000000000000]);
const DIFFICULTY_1_TARGET: (u64, i16) = (0xffffu64, 208); // 0xffff 2^208
const LOG_RATE: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct ShareStats {
    pub accepted: AtomicU64,
    pub stale: AtomicU64,
    pub low_diff: AtomicU64,
    pub duplicate: AtomicU64,
}

impl Display for ShareStats {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Shares: {}{}{}{}",
            match self.accepted.load(Ordering::SeqCst) {
                0 => "".to_string(),
                v => format!("Accepted: {} ", v),
            },
            match self.stale.load(Ordering::SeqCst) {
                0 => "".to_string(),
                v => format!("Stale: {} ", v),
            },
            match self.low_diff.load(Ordering::SeqCst) {
                0 => "".to_string(),
                v => format!("Low difficulty: {} ", v),
            },
            match self.duplicate.load(Ordering::SeqCst) {
                0 => "".to_string(),
                v => format!("Duplicate: {} ", v),
            }
        )
    }
}

#[allow(dead_code)]
pub struct StratumHandler {
    log_handler: JoinHandle<()>,

    //client: Framed<TcpStream, NewLineJsonCodec>,
    send_channel: Sender<StratumLine>,
    stream: Pin<Box<dyn Stream<Item = Result<StratumLine, NewLineJsonCodecError>>>>,
    miner_address: String,
    mine_when_not_synced: bool,
    devfund_address: Option<String>,
    devfund_percent: u16,
    mining_dev: Option<bool>,
    block_template_ctr: Arc<AtomicU16>,

    target_pool: Uint256,
    target_real: Uint256,
    nonce_mask: u64,
    nonce_fixed: u64,
    extranonce: Option<String>,
    last_stratum_id: Arc<AtomicU32>,

    shares_pending: Arc<Mutex<HashMap<u32, u64>>>,
    shares_stats: Arc<ShareStats>,
}

#[async_trait(?Send)]
impl Client for StratumHandler {
    fn add_devfund(&mut self, address: String, percent: u16) {
        self.devfund_address = Some(address);
        self.devfund_percent = percent;
    }

    async fn register(&mut self) -> Result<(), Error> {
        let mut id = { self.last_stratum_id.fetch_add(1, Ordering::SeqCst) };
        self.send_channel.send(
            StratumLine::StratumCommand(
                StratumCommand::Subscribe{
                    id,
                    params: (
                        env!("CARGO_PKG_VERSION").into(),
                        //self.extranonce.clone().unwrap_or("0xffffffff".into())
                    ),
                    error: None
                }
            )
        ).await?;
        id = self.last_stratum_id.fetch_add(1, Ordering::SeqCst);

        let pay_address = match &self.devfund_address {
            Some(devfund_address) if self.block_template_ctr.load(Ordering::SeqCst) <= self.devfund_percent => {
                self.mining_dev = Some(true);
                info!("Mining to devfund");
                devfund_address.clone()
            }
            _ => {
                self.mining_dev = Some(false);
                self.miner_address.clone()
            },
        };
        self.send_channel.send(StratumLine::StratumCommand(StratumCommand::Authorize{id, params: (pay_address.clone(), "x".into()), error: None })).await?;
        Ok(())
    }

    async fn listen(&mut self, miner: &mut MinerManager) -> Result<(), Error> {
        info!("Waiting for stuff");
        loop {
            {
                if (
                    !self.mining_dev.unwrap_or(true) &&
                        self.block_template_ctr.load(Ordering::SeqCst) <= self.devfund_percent
                ) || (
                    self.mining_dev.unwrap_or(false) &&
                        self.block_template_ctr.load(Ordering::SeqCst) > self.devfund_percent
                ) {
                    return Ok(());
                }
            }
            match self.stream.try_next().await? {
                Some(msg) => {
                    self.handle_message(msg, miner).await?
                },
                None => return Err("stratum message payload is empty".into()),
            }
        }
    }

    fn get_send_channel(&self) -> Sender<BlockSeed> {
        let (send, recv) = mpsc::channel::<BlockSeed>(1);
        let forwarding = self.send_channel.clone();
        let address = self.miner_address.clone();
        let stratum_id = self.last_stratum_id.clone();

        let shares_pending = self.shares_pending.clone();
        tokio::spawn(async move {
            ReceiverStream::new(recv).map(move |block_seed| {
                let (nonce, id) = match block_seed {
                    BlockSeed::PartialBlock { nonce, id, .. } => (nonce, id),
                    BlockSeed::FullBlock( _ ) => unreachable!(),
                };
                let msg_id = stratum_id.fetch_add(1, Ordering::SeqCst);
                {
                    shares_pending.try_lock().unwrap()
                        .insert(
                            msg_id,
                            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
                        );
                }
                StratumLine::StratumCommand(StratumCommand::MiningSubmit{
                    id: msg_id,
                    params: (address.clone(), id.into(), format!("{:#08x}", nonce)),
                    error: None
                })
            }).map(Ok).forward(PollSender::new(forwarding)).await
        });
        send
    }
}

impl StratumHandler {
    pub async fn connect(address: String, miner_address: String, mine_when_not_synced: bool, block_template_ctr: Option<Arc<AtomicU16>>) -> Result<Box<Self>, Error>
    {
        info!("Connecting to {}", address);
        let socket = TcpStream::connect(address).await.unwrap();

        let client = Framed::new(socket, NewLineJsonCodec::new());
        let (send_channel, recv) = mpsc::channel::<StratumLine>(3);
        let (sink, stream) = client.split();
        tokio::spawn(async move {
            ReceiverStream::new(recv).map(Ok).forward(sink).await
        });

        let shares_stats = Arc::new(ShareStats::default());
        Ok(Box::new(Self {
            log_handler: task::spawn(Self::log_shares(shares_stats.clone())),
            stream: Box::pin(stream),
            send_channel,
            miner_address,
            mine_when_not_synced,
            devfund_address: None,
            devfund_percent: 0,
            block_template_ctr: block_template_ctr.unwrap_or(Arc::new(AtomicU16::new((thread_rng().next_u64() % 10_000u64) as u16))),
            target_pool: Default::default(),
            target_real: Default::default(),
            nonce_mask: 0,
            nonce_fixed: 0,
            extranonce: None,
            last_stratum_id: Arc::new(AtomicU32::new(0)),
            shares_pending: Arc::new(Mutex::new(HashMap::<u32, u64>::new())),
            shares_stats,
            mining_dev: None
        }))
    }

    async fn handle_message(&mut self, msg: StratumLine, miner: &mut MinerManager) -> Result<(), Error> {
        match msg.clone() {
            StratumLine::StratumResult { id, error: None, .. } => {
                if let Some(_timestamp) = self.shares_pending.try_lock().unwrap().remove(&id) {
                    self.shares_stats.accepted.fetch_add(1, Ordering::SeqCst);
                    info!("Share accepted");
                } else {
                    info!("{:?} (Last: {})", msg.clone(), self.last_stratum_id.load(Ordering::SeqCst));
                    warn!("Ignoring result for now");
                }
                Ok(())
            }
            StratumLine::StratumResult { id, error: Some((code, error, _)), .. } => {
                let _timestamp = { self.shares_pending.try_lock().unwrap().remove(&id) };
                match code {
                    ErrorCode::Unknown => {
                        error!("Got error code {}: {}", code, error);
                        Err(error.into())
                    }
                    ErrorCode::JobNotFound => {
                        self.shares_stats.stale.fetch_add(1, Ordering::SeqCst);
                        warn!("Stale share");
                        Ok(())
                    }
                    ErrorCode::DuplicateShare => {
                        self.shares_stats.duplicate.fetch_add(1, Ordering::SeqCst);
                        warn!("Duplicate share");
                        Ok(())
                    }
                    ErrorCode::LowDifficultyShare => {
                        self.shares_stats.low_diff.fetch_add(1, Ordering::SeqCst);
                        warn!("Low difficulty share");
                        Ok(())
                    }
                    ErrorCode::Unauthorized => {
                        error!("Got error code {}: {}", code, error);
                        Err(error.into())
                    }
                    ErrorCode::NotSubscribed => {
                        error!("Got error code {}: {}", code, error);
                        Err(error.into())
                    }
                }
            }
            StratumLine::StratumCommand(StratumCommand::SetExtranonce { params: (ref extranonce, ref nonce_size), ref error, .. }) if error.is_none() => {
                self.set_extranonce(extranonce, nonce_size)
            },
            StratumLine::StratumCommand(StratumCommand::MiningSetDifficulty { params: (ref difficulty,), ref error, .. }) if error.is_none() => {
                self.set_difficulty(difficulty)
            },
            StratumLine::StratumCommand(StratumCommand::MiningNotify(MiningNotify::MiningNotifyShort{ params: (id, header_hash, timestamp), ref error, .. })) if error.is_none() => {
                self.block_template_ctr.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some((v+1) % 10_000)).unwrap();
                miner.process_block(Some(PartialBlock{
                    id, header_hash, timestamp, nonce: 0, target: self.target_pool.clone(),
                    nonce_mask: self.nonce_mask, nonce_fixed: self.nonce_fixed
                })).await
            },
            StratumLine::SubscribeResult { result: (ref _subscriptions, ref extranonce, ref nonce_size),.. } => {
                self.set_extranonce(extranonce, nonce_size)
                /*for (name, value) in _subscriptions {
                    match name.as_str() {
                        "mining.set_difficulty" => {self.set_difficulty(&f32::from_str(value.as_str())?)?;},
                        _ => {warn!("Ignored {} (={})", name, value);}
                    }
                }
                Ok(())*/
            },
            _ => Err(format!("Unhandled stratum response: {:?}", msg).into()),
        }
    }

    fn set_difficulty(&mut self, difficulty: &f32) -> Result<(),Error>{
        let mut buf = [0u64, 0u64, 0u64, 0u64];
        let (mantissa, exponent, _) = difficulty.recip().integer_decode();
        let new_mantissa = mantissa*DIFFICULTY_1_TARGET.0;
        let new_exponent = (DIFFICULTY_1_TARGET.1 + exponent) as u64;
        let start = (new_exponent / 64) as usize;
        let remainder = new_exponent % 64;

        buf[start] = new_mantissa << remainder;        // bottom
        if start < 3 {
            buf[start + 1] = new_mantissa >> 64 - remainder; // top
        } else if new_mantissa.leading_zeros() < remainder as u32 {
            return Err("Target is too big".into());
        }

        self.target_pool = Uint256::new(buf);
        info!("Difficulty: {:?}, Target: 0x{:x}", difficulty, self.target_pool);
        Ok(())
    }

    fn set_extranonce(&mut self, extranonce: &String, nonce_size: &u32) -> Result<(), Error>{
        self.extranonce = Some(extranonce.clone());
        self.nonce_fixed = u64::from_str_radix(extranonce.as_str(), 16)? << (nonce_size*8);
        self.nonce_mask = (1 << (nonce_size*8))-1;
        Ok(())
    }

    async fn log_shares(shares_info: Arc<ShareStats>) {
        let mut ticker = tokio::time::interval(LOG_RATE);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut _last_instant = ticker.tick().await;
        loop {
            let _now = ticker.tick().await;
            info!("{}", shares_info)
        }
    }
}

impl Drop for StratumHandler {
    fn drop(&mut self) {
        self.log_handler.abort();
    }
}