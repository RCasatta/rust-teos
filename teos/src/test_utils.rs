// Part of this file is an adaptation of test_utils from rust-lightning's lightning_block_sync crate.
// The original piece of software can be found at https://github.com/rust-bitcoin/rust-lightning/blob/main/lightning-block-sync/src/test_utils.rs

/* This file is licensed under either of
 *  Apache License, Version 2.0, (LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0) or
 *  MIT license (LICENSE-MIT or http://opensource.org/licenses/MIT)
 * at your option.
*/

use bitcoin::blockdata::block::{Block, BlockHeader};
use bitcoin::blockdata::constants::genesis_block;
use bitcoin::blockdata::script::{Builder, Script};
use bitcoin::blockdata::transaction::{OutPoint, Transaction, TxIn, TxOut};
use bitcoin::hash_types::BlockHash;
use bitcoin::hash_types::Txid;
use bitcoin::hashes::hex::FromHex;
use bitcoin::hashes::Hash;
use bitcoin::network::constants::Network;
use bitcoin::util::hash::bitcoin_merkle_root;
use bitcoin::util::psbt::serialize::Deserialize;
use bitcoin::util::uint::Uint256;
use lightning_block_sync::poll::{Validate, ValidatedBlockHeader};
use lightning_block_sync::{
    AsyncBlockSourceResult, BlockHeaderData, BlockSource, BlockSourceError, UnboundedCache,
};
use rand::Rng;
use teos_common::appointment::Appointment;
use teos_common::cryptography::encrypt;

use crate::extended_appointment::ExtendedAppointment;

#[derive(Clone, Default, Debug)]
pub(crate) struct Blockchain {
    pub blocks: Vec<Block>,
    without_blocks: Option<std::ops::RangeFrom<usize>>,
    without_headers: bool,
    malformed_headers: bool,
}

impl Blockchain {
    pub fn default() -> Self {
        Blockchain::with_network(Network::Bitcoin)
    }

    pub fn with_network(network: Network) -> Self {
        let blocks = vec![genesis_block(network)];
        Self {
            blocks,
            ..Default::default()
        }
    }

    pub fn with_height(mut self, height: usize) -> Self {
        self.blocks.reserve_exact(height);
        let bits = BlockHeader::compact_target_from_u256(&Uint256::from_be_bytes([0xff; 32]));
        for i in 1..=height {
            let prev_block = &self.blocks[i - 1];
            let prev_blockhash = prev_block.block_hash();
            let time = prev_block.header.time + height as u32;
            self.blocks.push(Block {
                header: BlockHeader {
                    version: 0,
                    prev_blockhash,
                    merkle_root: Default::default(),
                    time,
                    bits,
                    nonce: 0,
                },
                txdata: vec![],
            });
        }
        self
    }

    pub fn with_height_and_txs(mut self, height: usize, tx_count: Option<u8>) -> Self {
        let tx_count = match tx_count {
            Some(x) => x,
            None => 10,
        };

        for _ in 1..=height {
            let mut txs = Vec::new();
            for _ in 0..tx_count {
                txs.push(get_random_tx());
            }

            self.generate_with_txs(txs);
        }

        self
    }

    pub fn without_blocks(self, range: std::ops::RangeFrom<usize>) -> Self {
        Self {
            without_blocks: Some(range),
            ..self
        }
    }

    pub fn without_headers(self) -> Self {
        Self {
            without_headers: true,
            ..self
        }
    }

    pub fn malformed_headers(self) -> Self {
        Self {
            malformed_headers: true,
            ..self
        }
    }

    pub fn fork_at_height(&self, height: usize) -> Self {
        assert!(height + 1 < self.blocks.len());
        let mut blocks = self.blocks.clone();
        let mut prev_blockhash = blocks[height].block_hash();
        for block in blocks.iter_mut().skip(height + 1) {
            block.header.prev_blockhash = prev_blockhash;
            block.header.nonce += 1;
            prev_blockhash = block.block_hash();
        }
        Self {
            blocks,
            without_blocks: None,
            ..*self
        }
    }

    pub fn at_height(&self, height: usize) -> ValidatedBlockHeader {
        let block_header = self.at_height_unvalidated(height);
        let block_hash = self.blocks[height].block_hash();
        block_header.validate(block_hash).unwrap()
    }

    fn at_height_unvalidated(&self, height: usize) -> BlockHeaderData {
        assert!(!self.blocks.is_empty());
        assert!(height < self.blocks.len());
        BlockHeaderData {
            chainwork: self.blocks[0].header.work() + Uint256::from_u64(height as u64).unwrap(),
            height: height as u32,
            header: self.blocks[height].header.clone(),
        }
    }

    pub fn tip(&self) -> ValidatedBlockHeader {
        assert!(!self.blocks.is_empty());
        self.at_height(self.blocks.len() - 1)
    }

    pub fn disconnect_tip(&mut self) -> Option<Block> {
        self.blocks.pop()
    }

    pub fn generate_with_txs(&mut self, txs: Vec<Transaction>) {
        let bits = BlockHeader::compact_target_from_u256(&Uint256::from_be_bytes([0xff; 32]));
        let prev_block = &self.blocks.last().unwrap();
        let prev_blockhash = prev_block.block_hash();
        let time = prev_block.header.time + 1 as u32;
        let hashes = txs.iter().map(|obj| obj.txid().as_hash());

        self.blocks.push(Block {
            header: BlockHeader {
                version: 0,
                prev_blockhash,
                merkle_root: bitcoin_merkle_root(hashes).into(),
                time,
                bits,
                nonce: 0,
            },
            txdata: txs,
        });
    }

    pub fn header_cache(&self, heights: std::ops::RangeInclusive<usize>) -> UnboundedCache {
        let mut cache = UnboundedCache::new();
        for i in heights {
            let value = self.at_height(i);
            let key = value.header.block_hash();
            assert!(cache.insert(key, value).is_none());
        }
        cache
    }

    pub async fn get_block_count(&self) -> usize {
        self.blocks.len()
    }
}

impl BlockSource for Blockchain {
    fn get_header<'a>(
        &'a mut self,
        header_hash: &'a BlockHash,
        _height_hint: Option<u32>,
    ) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
        Box::pin(async move {
            if self.without_headers {
                return Err(BlockSourceError::persistent("header not found"));
            }

            for (height, block) in self.blocks.iter().enumerate() {
                if block.header.block_hash() == *header_hash {
                    let mut header_data = self.at_height_unvalidated(height);
                    if self.malformed_headers {
                        header_data.header.time += 1;
                    }

                    return Ok(header_data);
                }
            }
            Err(BlockSourceError::transient("header not found"))
        })
    }

    fn get_block<'a>(
        &'a mut self,
        header_hash: &'a BlockHash,
    ) -> AsyncBlockSourceResult<'a, Block> {
        Box::pin(async move {
            for (height, block) in self.blocks.iter().enumerate() {
                if block.header.block_hash() == *header_hash {
                    if let Some(without_blocks) = &self.without_blocks {
                        if without_blocks.contains(&height) {
                            return Err(BlockSourceError::persistent("block not found"));
                        }
                    }

                    return Ok(block.clone());
                }
            }
            Err(BlockSourceError::transient("block not found"))
        })
    }

    fn get_best_block<'a>(&'a mut self) -> AsyncBlockSourceResult<'a, (BlockHash, Option<u32>)> {
        Box::pin(async move {
            match self.blocks.last() {
                None => Err(BlockSourceError::transient("empty chain")),
                Some(block) => {
                    let height = (self.blocks.len() - 1) as u32;
                    Ok((block.block_hash(), Some(height)))
                }
            }
        })
    }
}

pub(crate) fn get_random_tx() -> Transaction {
    let mut rng = rand::thread_rng();
    let prev_txid_bytes = rng.gen::<[u8; 32]>();

    Transaction {
        version: 2,
        lock_time: 0,
        input: vec![TxIn {
            previous_output: OutPoint::new(
                Txid::from_slice(&prev_txid_bytes).unwrap(),
                rng.gen_range(0..200),
            ),
            script_sig: Script::new(),
            witness: Vec::new(),
            sequence: 0,
        }],
        output: vec![TxOut {
            script_pubkey: Builder::new().push_int(1).into_script(),
            value: rng.gen_range(0..21000000000),
        }],
    }
}

pub(crate) fn generate_dummy_appointment(dispute_txid: Option<&Txid>) -> ExtendedAppointment {
    let dispute_txid = match dispute_txid {
        Some(l) => l.clone(),
        None => Txid::from_slice(&[1; 32]).unwrap(),
    };

    let tx_bytes = Vec::from_hex("010000000001010000000000000000000000000000000000000000000000000000000000000000ffffffff54038e830a1b4d696e656420627920416e74506f6f6c373432c2005b005e7a0ae3fabe6d6d7841cd582ead8ea5dd8e3de1173cae6fcd2a53c7362ebb7fb6f815604fe07cbe0200000000000000ac0e060005f90000ffffffff04d9476026000000001976a91411dbe48cc6b617f9c6adaf4d9ed5f625b1c7cb5988ac0000000000000000266a24aa21a9ed7248c6efddd8d99bfddd7f499f0b915bffa8253003cc934df1ff14a81301e2340000000000000000266a24b9e11b6d7054937e13f39529d6ad7e685e9dd4efa426f247d5f5a5bed58cdddb2d0fa60100000000000000002b6a2952534b424c4f434b3a054a68aa5368740e8b3e3c67bce45619c2cfd07d4d4f0936a5612d2d0034fa0a0120000000000000000000000000000000000000000000000000000000000000000000000000").unwrap();
    let penalty_tx = Transaction::deserialize(&tx_bytes).unwrap();

    let mut locator = [0; 16];
    locator.copy_from_slice(&dispute_txid[..16]);

    let encrypted_blob = encrypt(&penalty_tx, &dispute_txid).unwrap();
    let appointment = Appointment::new(locator, encrypted_blob, 21);
    let user_id = [2; 16];
    let user_signature = [5, 6, 7, 8].to_vec();
    let start_block = 42;

    ExtendedAppointment::new(appointment, user_id, user_signature, start_block)
}