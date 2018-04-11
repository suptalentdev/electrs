#[macro_use]
extern crate log;

extern crate bitcoin;
extern crate byteorder;
extern crate crypto;
extern crate simple_logger;

mod daemon;
mod store;
mod timer;
mod waiter;

use bitcoin::blockdata::block::Block;
use bitcoin::network::serialize::BitcoinHash;
use bitcoin::network::serialize::{deserialize, serialize};
use bitcoin::util::hash::Sha256dHash;
use byteorder::{LittleEndian, WriteBytesExt};
use crypto::digest::Digest;
use crypto::sha2::Sha256;
use store::{Row, Store, StoreOptions};
use timer::Timer;

const HASH_LEN: usize = 8;

type Bytes = Vec<u8>;

fn index_block(block: &Block, height: usize) -> Vec<Row> {
    let null_hash = Sha256dHash::default();
    let mut rows = Vec::new();
    for tx in &block.txdata {
        let txid: Sha256dHash = tx.txid();
        for input in &tx.input {
            if input.prev_hash == null_hash {
                continue;
            }
            let mut key = Vec::<u8>::new(); // ???
            key.push(b'I');
            key.extend_from_slice(&input.prev_hash[..HASH_LEN]);
            key.write_u16::<LittleEndian>(input.prev_index as u16)
                .unwrap();
            rows.push(Row {
                key: key,
                value: txid[..HASH_LEN].to_vec(),
            });
        }
        for output in &tx.output {
            let mut script_hash = [0u8; 32];
            let mut sha2 = Sha256::new();
            sha2.input(&output.script_pubkey[..]);
            sha2.result(&mut script_hash);

            let mut key = Vec::<u8>::new(); // ???
            key.push(b'O');
            key.extend_from_slice(&script_hash);
            key.extend_from_slice(&txid[..HASH_LEN]);
            rows.push(Row {
                key: key,
                value: vec![],
            });
        }
        // Persist transaction ID and confirmed height
        {
            let mut key = Vec::<u8>::new();
            key.push(b'T');
            key.extend_from_slice(&txid[..]);
            let mut value = Vec::<u8>::new();
            value.write_u32::<LittleEndian>(height as u32).unwrap();
            rows.push(Row {
                key: key,
                value: value,
            })
        }
    }
    // Persist block hash and header
    {
        let mut key = Vec::<u8>::new();
        key.push(b'B');
        key.extend_from_slice(&block.bitcoin_hash()[..]);
        rows.push(Row {
            key: key,
            value: serialize(&block.header).unwrap(),
        })
    }
    rows
}

fn index_blocks(store: &mut Store) {
    let indexed_headers = store.read_headers();
    info!("indexed {} headers", indexed_headers.len());

    let (headers, blockhash) = daemon::get_headers();
    let mut hashes: Vec<(usize, String)> = daemon::enumerate_headers(&headers, &blockhash);
    hashes.retain(|item| !indexed_headers.contains_key(&item.1));
    info!("indexing {} blocks", hashes.len());

    let mut timer = Timer::new();

    let mut blocks_size = 0usize;
    let mut rows_size = 0usize;
    let mut num_of_rows = 0usize;

    for (height, blockhash) in hashes {
        timer.start("get");
        let buf: Bytes = daemon::get_bin(&format!("block/{}.bin", &blockhash));

        timer.start("parse");
        let block: Block = deserialize(&buf).unwrap();
        assert_eq!(&block.bitcoin_hash().be_hex_string(), &blockhash);

        timer.start("index");
        let rows = index_block(&block, height);
        for row in &rows {
            rows_size += row.key.len() + row.value.len();
        }
        num_of_rows += rows.len();

        timer.start("store");
        store.persist(rows);

        timer.stop();
        blocks_size += buf.len();

        debug!(
            "{} @ {}: {:.3}/{:.3} MB, {} rows, {}",
            blockhash,
            height,
            rows_size as f64 / 1e6_f64,
            blocks_size as f64 / 1e6_f64,
            num_of_rows,
            timer.stats()
        );
    }
    store.flush();
}

fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();
    let waiter = waiter::Waiter::new("tcp://localhost:28332");
    {
        let mut store = Store::open(
            "db/mainnet",
            StoreOptions {
                auto_compact: false,
            },
        );
        index_blocks(&mut store);
        store.compact_if_needed();
    }

    let mut store = Store::open("db/mainnet", StoreOptions { auto_compact: true });
    loop {
        if store.has_block(&waiter.wait()) {
            continue;
        }
        index_blocks(&mut store);
    }
}
