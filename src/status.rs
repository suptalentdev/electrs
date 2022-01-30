use anyhow::Result;
use bitcoin::{
    hashes::{sha256, Hash, HashEngine},
    Amount, Block, BlockHash, OutPoint, Transaction, Txid,
};
use rayon::prelude::*;
use serde_json::{json, Value};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryFrom;

use crate::{
    cache::Cache,
    chain::Chain,
    daemon::Daemon,
    index::Index,
    mempool::Mempool,
    merkle::Proof,
    types::{ScriptHash, StatusHash},
};

#[derive(Default)]
struct Entry {
    outputs: Vec<u32>,
    spent: Vec<OutPoint>,
}

struct TxEntry {
    txid: Txid,
    outputs: Vec<u32>,
    spent: Vec<OutPoint>,
}

impl TxEntry {
    fn new(txid: Txid, entry: Entry) -> Self {
        Self {
            txid,
            outputs: entry.outputs,
            spent: entry.spent,
        }
    }
}

pub(crate) struct ConfirmedEntry {
    txid: Txid,
    height: usize,
}

impl ConfirmedEntry {
    pub fn hash(&self, engine: &mut sha256::HashEngine) {
        let s = format!("{}:{}:", self.txid, self.height);
        engine.input(s.as_bytes());
    }

    pub fn value(&self) -> Value {
        json!({"tx_hash": self.txid, "height": self.height})
    }
}

pub(crate) struct MempoolEntry {
    txid: Txid,
    has_unconfirmed_inputs: bool,
    fee: Amount,
}

impl MempoolEntry {
    fn height(&self) -> isize {
        if self.has_unconfirmed_inputs {
            -1
        } else {
            0
        }
    }

    pub fn hash(&self, engine: &mut sha256::HashEngine) {
        let s = format!("{}:{}:", self.txid, self.height());
        engine.input(s.as_bytes());
    }

    pub fn value(&self) -> Value {
        json!({"tx_hash": self.txid, "height": self.height(), "fee": self.fee.as_sat()})
    }
}

/// ScriptHash subscription status
pub struct Status {
    scripthash: ScriptHash,
    tip: BlockHash,
    statushash: Option<StatusHash>,
    confirmed: HashMap<BlockHash, Vec<TxEntry>>,
    mempool: Vec<TxEntry>,
}

fn make_outpoints<'a>(txid: &'a Txid, outputs: &'a [u32]) -> impl Iterator<Item = OutPoint> + 'a {
    outputs.iter().map(move |vout| OutPoint::new(*txid, *vout))
}

impl Status {
    pub fn new(scripthash: ScriptHash) -> Self {
        Self {
            scripthash,
            tip: BlockHash::default(),
            statushash: None,
            confirmed: HashMap::new(),
            mempool: Vec::new(),
        }
    }

    fn filter_outputs(&self, tx: &Transaction) -> Vec<u32> {
        let outputs = tx.output.iter().zip(0u32..);
        outputs
            .filter_map(move |(txo, vout)| {
                if ScriptHash::new(&txo.script_pubkey) == self.scripthash {
                    Some(vout)
                } else {
                    None
                }
            })
            .collect()
    }

    fn filter_inputs(&self, tx: &Transaction, outpoints: &HashSet<OutPoint>) -> Vec<OutPoint> {
        tx.input
            .iter()
            .filter_map(|txi| {
                if outpoints.contains(&txi.previous_output) {
                    Some(txi.previous_output)
                } else {
                    None
                }
            })
            .collect()
    }

    fn funding_confirmed(&self, chain: &Chain) -> HashSet<OutPoint> {
        self.confirmed
            .iter()
            .filter_map(|(blockhash, entries)| chain.get_block_height(blockhash).map(|_| entries))
            .flat_map(|entries| {
                entries
                    .iter()
                    .flat_map(|entry| make_outpoints(&entry.txid, &entry.outputs))
            })
            .collect()
    }

    pub(crate) fn get_unspent(&self, chain: &Chain) -> HashSet<OutPoint> {
        let mut unspent: HashSet<OutPoint> = self.funding_confirmed(chain);
        unspent.extend(
            self.mempool
                .iter()
                .flat_map(|entry| make_outpoints(&entry.txid, &entry.outputs)),
        );

        let spent_outpoints = self
            .confirmed
            .iter()
            .filter_map(|(blockhash, entries)| {
                chain.get_block_height(blockhash).map(|_height| entries)
            })
            .flatten()
            .chain(self.mempool.iter())
            .flat_map(|entry| entry.spent.iter());
        for outpoint in spent_outpoints {
            assert!(unspent.remove(outpoint), "missing outpoint {}", outpoint);
        }
        unspent
    }

    pub(crate) fn get_confirmed(&self, chain: &Chain) -> Vec<ConfirmedEntry> {
        self.confirmed
            .iter()
            .filter_map(move |(blockhash, entries)| {
                chain
                    .get_block_height(blockhash)
                    .map(|height| (height, &entries[..]))
            })
            .collect::<BTreeMap<usize, &[TxEntry]>>()
            .into_iter()
            .flat_map(|(height, entries)| {
                entries.iter().map(move |e| ConfirmedEntry {
                    txid: e.txid,
                    height,
                })
            })
            .collect()
    }

    pub(crate) fn get_mempool(&self, mempool: &Mempool) -> Vec<MempoolEntry> {
        let mut entries = self
            .mempool
            .iter()
            .filter_map(|e| mempool.get(&e.txid))
            .collect::<Vec<_>>();
        entries.sort_by_key(|e| (e.has_unconfirmed_inputs, e.txid));
        entries
            .into_iter()
            .map(|e| MempoolEntry {
                txid: e.txid,
                has_unconfirmed_inputs: e.has_unconfirmed_inputs,
                fee: e.fee,
            })
            .collect()
    }

    fn for_new_blocks<B, F>(&self, blockhashes: B, daemon: &Daemon, func: F) -> Result<()>
    where
        B: IntoIterator<Item = BlockHash>,
        F: FnMut(BlockHash, Block),
    {
        daemon.for_blocks(
            blockhashes
                .into_iter()
                .filter(|blockhash| !self.confirmed.contains_key(blockhash)),
            func,
        )
    }

    fn sync_confirmed(
        &self,
        index: &Index,
        daemon: &Daemon,
        cache: &Cache,
        outpoints: &mut HashSet<OutPoint>,
    ) -> Result<HashMap<BlockHash, Vec<TxEntry>>> {
        type PosTxid = (u32, Txid);
        let mut result = HashMap::<BlockHash, HashMap<PosTxid, Entry>>::new();

        let funding_blockhashes = index.filter_by_funding(self.scripthash);
        self.for_new_blocks(funding_blockhashes, daemon, |blockhash, block| {
            let txids: Vec<Txid> = block.txdata.iter().map(|tx| tx.txid()).collect();
            for (pos, (tx, txid)) in block.txdata.into_iter().zip(txids.iter()).enumerate() {
                let funding_outputs = self.filter_outputs(&tx);
                if funding_outputs.is_empty() {
                    continue;
                }
                cache.add_tx(*txid, move || tx);
                cache.add_proof(blockhash, *txid, || Proof::create(&txids, pos));
                outpoints.extend(make_outpoints(&txid, &funding_outputs));
                result
                    .entry(blockhash)
                    .or_default()
                    .entry((u32::try_from(pos).unwrap(), *txid))
                    .or_default()
                    .outputs = funding_outputs;
            }
        })?;
        let spending_blockhashes: HashSet<BlockHash> = outpoints
            .par_iter()
            .flat_map_iter(|outpoint| index.filter_by_spending(*outpoint))
            .collect();
        self.for_new_blocks(
            spending_blockhashes.into_iter(),
            daemon,
            |blockhash, block| {
                let txids: Vec<Txid> = block.txdata.iter().map(|tx| tx.txid()).collect();
                for (pos, (tx, txid)) in block.txdata.into_iter().zip(txids.iter()).enumerate() {
                    let spent_outpoints = self.filter_inputs(&tx, &outpoints);
                    if spent_outpoints.is_empty() {
                        continue;
                    }
                    cache.add_tx(*txid, move || tx);
                    cache.add_proof(blockhash, *txid, || Proof::create(&txids, pos));
                    result
                        .entry(blockhash)
                        .or_default()
                        .entry((u32::try_from(pos).unwrap(), *txid))
                        .or_default()
                        .spent = spent_outpoints;
                }
            },
        )?;

        Ok(result
            .into_iter()
            .map(|(blockhash, entries_map)| {
                let sorted_entries = entries_map
                    .into_iter()
                    .collect::<BTreeMap<PosTxid, Entry>>()
                    .into_iter()
                    .map(|((_pos, txid), entry)| TxEntry::new(txid, entry))
                    .collect::<Vec<TxEntry>>();
                (blockhash, sorted_entries)
            })
            .collect())
    }

    fn sync_mempool(
        &self,
        mempool: &Mempool,
        cache: &Cache,
        outpoints: &mut HashSet<OutPoint>,
    ) -> Vec<TxEntry> {
        let mut result = HashMap::<Txid, Entry>::new();
        for entry in mempool.filter_by_funding(&self.scripthash) {
            let funding_outputs = self.filter_outputs(&entry.tx);
            assert!(!funding_outputs.is_empty());
            outpoints.extend(make_outpoints(&entry.txid, &funding_outputs));
            result.entry(entry.txid).or_default().outputs = funding_outputs;
            cache.add_tx(entry.txid, || entry.tx.clone());
        }
        for entry in outpoints
            .iter()
            .flat_map(|outpoint| mempool.filter_by_spending(outpoint))
        {
            let spent_outpoints = self.filter_inputs(&entry.tx, &outpoints);
            assert!(!spent_outpoints.is_empty());
            result.entry(entry.txid).or_default().spent = spent_outpoints;
            cache.add_tx(entry.txid, || entry.tx.clone());
        }
        result
            .into_iter()
            .map(|(txid, entry)| TxEntry::new(txid, entry))
            .collect()
    }

    fn compute_status_hash(&self, chain: &Chain, mempool: &Mempool) -> Option<StatusHash> {
        let confirmed = self.get_confirmed(chain);
        let mempool = self.get_mempool(mempool);
        if confirmed.is_empty() && mempool.is_empty() {
            return None;
        }
        let mut engine = StatusHash::engine();
        for entry in confirmed {
            entry.hash(&mut engine);
        }
        for entry in mempool {
            entry.hash(&mut engine);
        }
        Some(StatusHash::from_engine(engine))
    }

    pub(crate) fn sync(
        &mut self,
        index: &Index,
        mempool: &Mempool,
        daemon: &Daemon,
        cache: &Cache,
    ) -> Result<()> {
        let mut outpoints: HashSet<OutPoint> = self.funding_confirmed(index.chain());

        let new_tip = index.chain().tip();
        if self.tip != new_tip {
            let update = self.sync_confirmed(index, daemon, cache, &mut outpoints)?;
            self.confirmed.extend(update);
            self.tip = new_tip;
        }
        if !self.confirmed.is_empty() {
            debug!(
                "{} transactions from {} blocks",
                self.confirmed.values().map(Vec::len).sum::<usize>(),
                self.confirmed.len()
            );
        }
        self.mempool = self.sync_mempool(mempool, cache, &mut outpoints);
        if !self.mempool.is_empty() {
            debug!("{} mempool transactions", self.mempool.len());
        }
        self.statushash = self.compute_status_hash(index.chain(), mempool);
        Ok(())
    }

    pub fn statushash(&self) -> Option<StatusHash> {
        self.statushash
    }
}
