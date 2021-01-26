use graph::{
    constraint_violation,
    prelude::{ethabi, tiny_keccak, ChainStore as ChainStoreTrait, EthereumCallCache, StoreError},
};

use diesel::sql_types::Text;
use diesel::{dsl::sql, pg::PgConnection};
use diesel::{insert_into, select, update};
use diesel::{prelude::*, sql_query};
use diesel::{
    r2d2::{ConnectionManager, PooledConnection},
    sql_types::BigInt,
};

use graph::ensure;
use std::sync::Arc;
use std::{collections::HashMap, convert::TryFrom};
use std::{convert::TryInto, iter::FromIterator};

use graph::prelude::{
    serde_json, web3::types::H256, BlockNumber, ChainHeadUpdateListener as _,
    ChainHeadUpdateStream, Error, EthereumBlock, EthereumBlockPointer, EthereumNetworkIdentifier,
    Future, LightEthereumBlock, Stream,
};

//use web3::types::H256;

use crate::functions::lookup_ancestor_block;
use crate::{chain_head_listener::ChainHeadUpdateListener, connection_pool::ConnectionPool};

/// Tables in the 'public' database schema that store chain-specific data
mod public {
    table! {
        ethereum_networks (name) {
            name -> Varchar,
            head_block_hash -> Nullable<Varchar>,
            head_block_number -> Nullable<BigInt>,
            net_version -> Nullable<Varchar>,
            genesis_block_hash -> Nullable<Varchar>,
        }
    }

    table! {
        ethereum_blocks (hash) {
            hash -> Varchar,
            number -> BigInt,
            parent_hash -> Nullable<Varchar>,
            network_name -> Varchar, // REFERENCES ethereum_networks (name),
            data -> Jsonb,
        }
    }

    allow_tables_to_appear_in_same_query!(ethereum_networks, ethereum_blocks);

    table! {
        /// `id` is the hash of contract address + encoded function call + block number.
        eth_call_cache (id) {
            id -> Bytea,
            return_value -> Bytea,
            contract_address -> Bytea,
            block_number -> Integer,
        }
    }

    table! {
        /// When was a cached call on a contract last used? This is useful to clean old data.
        eth_call_meta (contract_address) {
            contract_address -> Bytea,
            accessed_at -> Date,
        }
    }

    joinable!(eth_call_cache -> eth_call_meta (contract_address));
    allow_tables_to_appear_in_same_query!(eth_call_cache, eth_call_meta);
}

pub struct ChainStore {
    conn: ConnectionPool,
    network: String,
    genesis_block_ptr: EthereumBlockPointer,
    chain_head_update_listener: Arc<ChainHeadUpdateListener>,
}

impl ChainStore {
    pub fn new(
        network: String,
        net_identifier: EthereumNetworkIdentifier,
        chain_head_update_listener: Arc<ChainHeadUpdateListener>,
        pool: ConnectionPool,
    ) -> Self {
        let store = ChainStore {
            conn: pool,
            network,
            genesis_block_ptr: (net_identifier.genesis_block_hash, 0 as u64).into(),
            chain_head_update_listener,
        };

        // Add network to store and check network identifiers
        store.add_network_if_missing(net_identifier).unwrap();

        store
    }

    fn get_conn(&self) -> Result<PooledConnection<ConnectionManager<PgConnection>>, Error> {
        self.conn.get().map_err(Error::from)
    }

    fn add_network_if_missing(
        &self,
        new_net_identifiers: EthereumNetworkIdentifier,
    ) -> Result<(), Error> {
        use public::ethereum_networks::dsl::*;

        let new_genesis_block_hash = new_net_identifiers.genesis_block_hash;
        let new_net_version = new_net_identifiers.net_version;

        let network_identifiers_opt = ethereum_networks
            .select((net_version, genesis_block_hash))
            .filter(name.eq(&self.network))
            .first::<(Option<String>, Option<String>)>(&*self.get_conn()?)
            .optional()?;

        match network_identifiers_opt {
            // Network is missing in database
            None => {
                insert_into(ethereum_networks)
                    .values((
                        name.eq(&self.network),
                        head_block_hash.eq::<Option<String>>(None),
                        head_block_number.eq::<Option<i64>>(None),
                        net_version.eq::<Option<String>>(Some(new_net_version.to_owned())),
                        genesis_block_hash
                            .eq::<Option<String>>(Some(format!("{:x}", new_genesis_block_hash))),
                    ))
                    .on_conflict(name)
                    .do_nothing()
                    .execute(&*self.get_conn()?)?;
            }

            // Network is in database and has identifiers
            Some((Some(last_net_version), Some(last_genesis_block_hash))) => {
                if last_net_version != new_net_version {
                    panic!(
                        "Ethereum node provided net_version {}, \
                         but we expected {}. Did you change networks \
                         without changing the network name?",
                        new_net_version, last_net_version
                    );
                }

                if last_genesis_block_hash.parse().ok() != Some(new_genesis_block_hash) {
                    panic!(
                        "Ethereum node provided genesis block hash {}, \
                         but we expected {}. Did you change networks \
                         without changing the network name?",
                        new_genesis_block_hash, last_genesis_block_hash
                    );
                }
            }

            // Network is in database but is missing identifiers
            Some(_) => {
                update(ethereum_networks)
                    .set((
                        net_version.eq::<Option<String>>(Some(new_net_version.to_owned())),
                        genesis_block_hash
                            .eq::<Option<String>>(Some(format!("{:x}", new_genesis_block_hash))),
                    ))
                    .filter(name.eq(&self.network))
                    .execute(&*self.get_conn()?)?;
            }
        }

        Ok(())
    }

    pub fn chain_head_pointers(&self) -> Result<HashMap<String, EthereumBlockPointer>, StoreError> {
        use public::ethereum_networks as n;

        let pointers: Vec<(String, EthereumBlockPointer)> = n::table
            .select((n::name, n::head_block_hash, n::head_block_number))
            .load::<(String, Option<String>, Option<i64>)>(&self.get_conn()?)?
            .into_iter()
            .filter_map(|(name, hash, number)| match (hash, number) {
                (Some(hash), Some(number)) => Some((name, hash, number)),
                _ => None,
            })
            .map(|(name, hash, number)| {
                EthereumBlockPointer::try_from((hash.as_str(), number)).map(|ptr| (name, ptr))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(HashMap::from_iter(pointers))
    }

    pub fn chain_head_block(&self, network: &str) -> Result<Option<u64>, StoreError> {
        use public::ethereum_networks as n;

        let number: Option<i64> = n::table
            .filter(n::name.eq(network))
            .select(n::head_block_number)
            .first::<Option<i64>>(&self.get_conn()?)
            .optional()?
            .flatten();

        number.map(|number| number.try_into()).transpose().map_err(
            |e: std::num::TryFromIntError| {
                constraint_violation!(
                    "head block number for {} is {:?} which does not fit into a u32: {}",
                    network,
                    number,
                    e.to_string()
                )
            },
        )
    }

    /// Find the first block that is missing from the database needed to
    /// complete the chain from block `hash` to the block with number
    /// `first_block`. We return the hash of the missing block as an
    /// array because the remaining code expects that, but the array will only
    /// ever have at most one element.
    fn missing_parents(
        &self,
        conn: &PgConnection,
        first_block: i64,
        hash: &str,
        genesis: &str,
    ) -> Result<Vec<H256>, Error> {
        // We recursively build a temp table 'chain' containing the hash and
        // parent_hash of blocks to check. The 'last' value is used to stop
        // the recursion and is true if one of these conditions is true:
        //   * we are missing a parent block
        //   * we checked the required number of blocks
        //   * we checked the genesis block
        const MISSING_PARENT_SQL: &str = "
            with recursive chain(hash, parent_hash, last) as (
                -- base case: look at the head candidate block
                select b.hash, b.parent_hash, false
                  from ethereum_blocks b
                 where b.network_name = $1
                   and b.hash = $2
                   and b.hash != $3
                union all
                -- recursion step: add a block whose hash is the latest parent_hash
                -- on chain
                select chain.parent_hash,
                       b.parent_hash,
                       coalesce(b.parent_hash is null
                             or b.number <= $4
                             or b.hash = $3, true)
                  from chain left outer join ethereum_blocks b
                              on chain.parent_hash = b.hash
                             and b.network_name = $1
                 where not chain.last)
             select hash
               from chain
              where chain.parent_hash is null;
            ";

        #[derive(QueryableByName)]
        struct MissingParent {
            #[sql_type = "Text"]
            hash: String,
        };

        let missing = sql_query(MISSING_PARENT_SQL)
            .bind::<Text, _>(&self.network)
            .bind::<Text, _>(&hash)
            .bind::<Text, _>(&genesis)
            .bind::<BigInt, _>(first_block)
            .load::<MissingParent>(conn)?;

        missing
            .into_iter()
            .map(|parent| parent.hash.parse())
            .collect::<Result<_, _>>()
            .map_err(Error::from)
    }
}

impl ChainStoreTrait for ChainStore {
    fn genesis_block_ptr(&self) -> Result<EthereumBlockPointer, Error> {
        Ok(self.genesis_block_ptr)
    }

    fn upsert_blocks<B, E>(
        &self,
        blocks: B,
    ) -> Box<dyn Future<Item = (), Error = E> + Send + 'static>
    where
        B: Stream<Item = EthereumBlock, Error = E> + Send + 'static,
        E: From<Error> + Send + 'static,
    {
        use public::ethereum_blocks::dsl::*;

        let conn = self.conn.clone();
        let net_name = self.network.clone();
        Box::new(blocks.for_each(move |block| {
            let json_blob = serde_json::to_value(&block).expect("Failed to serialize block");
            let values = (
                hash.eq(format!("{:x}", block.block.hash.unwrap())),
                number.eq(block.block.number.unwrap().as_u64() as i64),
                parent_hash.eq(format!("{:x}", block.block.parent_hash)),
                network_name.eq(&net_name),
                data.eq(json_blob),
            );

            // Insert blocks.
            //
            // If the table already contains a block with the same hash, then overwrite that block
            // if it may be adding transaction receipts.
            insert_into(ethereum_blocks)
                .values(values.clone())
                .on_conflict(hash)
                .do_update()
                .set(values)
                .execute(&*conn.get().map_err(Error::from)?)
                .map_err(Error::from)
                .map_err(E::from)
                .map(|_| ())
        }))
    }

    fn upsert_light_blocks(&self, blocks: Vec<LightEthereumBlock>) -> Result<(), Error> {
        use public::ethereum_blocks::dsl::*;

        let conn = self.conn.clone();
        let net_name = self.network.clone();
        for block in blocks {
            let block_hash = format!("{:x}", block.hash.unwrap());
            let p_hash = format!("{:x}", block.parent_hash);
            let block_number = block.number.unwrap().as_u64();
            let json_blob = serde_json::to_value(&EthereumBlock {
                block,
                transaction_receipts: Vec::new(),
            })
            .expect("Failed to serialize block");
            let values = (
                hash.eq(block_hash),
                number.eq(block_number as i64),
                parent_hash.eq(p_hash),
                network_name.eq(&net_name),
                data.eq(json_blob),
            );

            // Insert blocks. On conflict do nothing, we don't want to erase transaction receipts.
            insert_into(ethereum_blocks)
                .values(values.clone())
                .on_conflict(hash)
                .do_nothing()
                .execute(&*conn.get()?)?;
        }
        Ok(())
    }

    fn attempt_chain_head_update(&self, ancestor_count: u64) -> Result<Vec<H256>, Error> {
        use public::ethereum_blocks as b;
        use public::ethereum_networks as n;

        let conn = self.get_conn()?;
        let candidate = b::table
            .inner_join(n::table.on(b::network_name.eq(n::name)))
            .filter(n::name.eq(&self.network))
            .filter(b::number.gt(sql("coalesce(ethereum_networks.head_block_number, -1)")))
            .order_by((b::number.desc(), b::hash))
            .select((b::hash, b::number, n::genesis_block_hash))
            .first::<(String, i64, Option<String>)>(&conn)
            .optional()?;
        let (hash, number, first_block, genesis) = match candidate {
            None => return Ok(vec![]),
            Some((hash, number, genesis)) => {
                (hash, number, 0.max(number - ancestor_count as i64), genesis)
            }
        };
        let genesis = match genesis {
            None => {
                return Err(constraint_violation!(
                    "network `{}` has no genesis block hash",
                    &self.network
                )
                .into());
            }
            Some(g) => g,
        };

        let missing = self.missing_parents(&conn, first_block, &hash, &genesis)?;
        if !missing.is_empty() {
            return Ok(missing);
        }

        update(n::table.filter(n::name.eq(&self.network)))
            .set((
                n::head_block_hash.eq(&hash),
                n::head_block_number.eq(number),
            ))
            .execute(&conn)?;

        ChainHeadUpdateListener::send(&conn, &self.network, &hash, number)?;

        Ok(vec![])
    }

    fn chain_head_updates(&self) -> ChainHeadUpdateStream {
        self.chain_head_update_listener
            .subscribe(self.network.to_owned())
    }

    fn chain_head_ptr(&self) -> Result<Option<EthereumBlockPointer>, Error> {
        use public::ethereum_networks::dsl::*;

        ethereum_networks
            .select((head_block_hash, head_block_number))
            .filter(name.eq(&self.network))
            .load::<(Option<String>, Option<i64>)>(&*self.get_conn()?)
            .map(|rows| {
                rows.first()
                    .map(|(hash_opt, number_opt)| match (hash_opt, number_opt) {
                        (Some(hash), Some(number)) => Some((hash.parse().unwrap(), *number).into()),
                        (None, None) => None,
                        _ => unreachable!(),
                    })
                    .and_then(|opt| opt)
            })
            .map_err(Error::from)
    }

    fn blocks(&self, hashes: Vec<H256>) -> Result<Vec<LightEthereumBlock>, Error> {
        use diesel::dsl::any;
        use diesel::sql_types::Jsonb;
        use public::ethereum_blocks::dsl::*;

        ethereum_blocks
            .select(sql::<Jsonb>("data -> 'block'"))
            .filter(network_name.eq(&self.network))
            .filter(hash.eq(any(Vec::from_iter(
                hashes.into_iter().map(|h| format!("{:x}", h)),
            ))))
            .load::<serde_json::Value>(&*self.get_conn()?)?
            .into_iter()
            .map(|block| serde_json::from_value(block).map_err(Into::into))
            .collect()
    }

    fn ancestor_block(
        &self,
        block_ptr: EthereumBlockPointer,
        offset: u64,
    ) -> Result<Option<EthereumBlock>, Error> {
        ensure!(
            block_ptr.number >= offset,
            "block offset points to before genesis block"
        );

        select(lookup_ancestor_block(block_ptr.hash_hex(), offset as i64))
            .first::<Option<serde_json::Value>>(&*self.get_conn()?)
            .map(|val_opt| {
                val_opt.map(|val| {
                    serde_json::from_value::<EthereumBlock>(val)
                        .expect("Failed to deserialize block from database")
                })
            })
            .map_err(Error::from)
    }

    fn cleanup_cached_blocks(&self, ancestor_count: u64) -> Result<(BlockNumber, usize), Error> {
        use diesel::sql_types::Integer;
        use public::ethereum_blocks::dsl;

        #[derive(QueryableByName)]
        struct MinBlock {
            #[sql_type = "Integer"]
            block: i32,
        };

        // Remove all blocks from the cache that are behind the slowest
        // subgraph's head block, but retain the genesis block. We stay
        // behind the slowest subgraph so that we do not interfere with its
        // syncing activity.
        // We also stay `ancestor_count` many blocks behind the head of the
        // chain since the block ingestor consults these blocks frequently
        //
        // Only consider active subgraphs that have not failed

        // This assumes that subgraph metadata and blocks are stored in the
        // same shard. We disallow setting GRAPH_ETHEREUM_CLEANUP_BLOCKS in
        // graph_node::config so that we only run this query when we know
        // it will work. Running this with a sharded store might remove
        // blocks that are still needed by deployments in other shard
        //
        // See 8b6ad0c64e244023ac20ced7897fe666

        let conn = self.get_conn()?;
        let query = "
            select coalesce(
                   least(a.block,
                        (select head_block_number::int - $1
                           from ethereum_networks
                          where name = $2)), -1)::int as block
              from (
                select min(d.latest_ethereum_block_number) as block
                  from subgraphs.subgraph_deployment d,
                       subgraphs.subgraph_deployment_assignment a,
                       subgraphs.ethereum_contract_data_source ds
                 where left(ds.id, 46) = d.id
                   and a.id = d.id
                   and not d.failed
                   and ds.network = $2) a;";
        let ancestor_count = i32::try_from(ancestor_count)
            .expect("ancestor_count fits into a signed 32 bit integer");
        diesel::sql_query(query)
            .bind::<Integer, _>(ancestor_count)
            .bind::<Text, _>(&self.network)
            .load::<MinBlock>(&conn)?
            .first()
            .map(|MinBlock { block }| {
                // If we could not determine a minimum block, the query
                // returns -1, and we should not do anything. We also guard
                // against removing the genesis block
                if *block > 0 {
                    diesel::delete(dsl::ethereum_blocks)
                        .filter(dsl::network_name.eq(&self.network))
                        .filter(dsl::number.lt(*block as i64))
                        .filter(dsl::number.gt(0))
                        .execute(&conn)
                        .map(|rows| (*block, rows))
                } else {
                    Ok((0, 0))
                }
            })
            .unwrap_or(Ok((0, 0)))
            .map_err(|e| e.into())
    }

    fn block_hashes_by_block_number(&self, number: u64) -> Result<Vec<H256>, Error> {
        use public::ethereum_blocks::dsl;

        let conn = self.get_conn()?;
        dsl::ethereum_blocks
            .select(dsl::hash)
            .filter(dsl::network_name.eq(&self.network))
            .filter(dsl::number.eq(number as i64))
            .get_results::<String>(&conn)?
            .into_iter()
            .map(|h| h.parse())
            .collect::<Result<Vec<H256>, _>>()
            .map_err(Error::from)
    }

    fn confirm_block_hash(&self, number: u64, hash: &H256) -> Result<usize, Error> {
        use public::ethereum_blocks::dsl;

        let conn = self.get_conn()?;
        diesel::delete(dsl::ethereum_blocks)
            .filter(dsl::network_name.eq(&self.network))
            .filter(dsl::number.eq(number as i64))
            .filter(dsl::hash.ne(&format!("{:x}", hash)))
            .execute(&conn)
            .map_err(Error::from)
    }

    fn block_number(&self, hash: H256) -> Result<Option<(String, BlockNumber)>, StoreError> {
        use public::ethereum_blocks::dsl;

        let conn = self.get_conn()?;
        dsl::ethereum_blocks
            .select((dsl::network_name, dsl::number))
            .filter(dsl::hash.eq(format!("{:x}", hash)))
            .first::<(String, i64)>(&conn)
            .optional()?
            .map(|(name, number)| {
                BlockNumber::try_from(number)
                    .map(|number| (name, number))
                    .map_err(|e| StoreError::QueryExecutionError(e.to_string()))
            })
            .transpose()
    }
}

impl EthereumCallCache for ChainStore {
    fn get_call(
        &self,
        contract_address: ethabi::Address,
        encoded_call: &[u8],
        block: EthereumBlockPointer,
    ) -> Result<Option<Vec<u8>>, Error> {
        use public::{eth_call_cache, eth_call_meta};

        let id = contract_call_id(&contract_address, encoded_call, &block);
        let conn = &*self.get_conn()?;
        if let Some(call_output) = conn.transaction::<_, Error, _>(|| {
            if let Some((return_value, update_accessed_at)) = eth_call_cache::table
                .find(id.as_ref())
                .inner_join(eth_call_meta::table)
                .select((
                    eth_call_cache::return_value,
                    sql("CURRENT_DATE > eth_call_meta.accessed_at"),
                ))
                .get_result(conn)
                .optional()?
            {
                if update_accessed_at {
                    update(eth_call_meta::table.find(contract_address.as_ref()))
                        .set(eth_call_meta::accessed_at.eq(sql("CURRENT_DATE")))
                        .execute(conn)?;
                }
                Ok(Some(return_value))
            } else {
                Ok(None)
            }
        })? {
            Ok(Some(call_output))
        } else {
            // No entry with the new id format, try the old one.
            let old_id = old_contract_call_id(&contract_address, &encoded_call, &block);
            if let Some(return_value) = eth_call_cache::table
                .find(old_id.as_ref())
                .select(eth_call_cache::return_value)
                .get_result::<Vec<u8>>(conn)
                .optional()?
            {
                use public::eth_call_cache::dsl;

                // Migrate to the new format by re-inserting the call and deleting the old entry.
                self.set_call(contract_address, encoded_call, block, &return_value)?;
                diesel::delete(eth_call_cache::table.filter(dsl::id.eq(old_id.as_ref())))
                    .execute(conn)?;
                Ok(Some(return_value))
            } else {
                Ok(None)
            }
        }
    }

    fn set_call(
        &self,
        contract_address: ethabi::Address,
        encoded_call: &[u8],
        block: EthereumBlockPointer,
        return_value: &[u8],
    ) -> Result<(), Error> {
        use public::{eth_call_cache, eth_call_meta};

        let id = contract_call_id(&contract_address, encoded_call, &block);
        let conn = &*self.get_conn()?;
        conn.transaction(|| {
            insert_into(eth_call_cache::table)
                .values((
                    eth_call_cache::id.eq(id.as_ref()),
                    eth_call_cache::contract_address.eq(contract_address.as_ref()),
                    eth_call_cache::block_number.eq(block.number as i32),
                    eth_call_cache::return_value.eq(return_value),
                ))
                .on_conflict_do_nothing()
                .execute(conn)?;

            let accessed_at = eth_call_meta::accessed_at.eq(sql("CURRENT_DATE"));
            insert_into(eth_call_meta::table)
                .values((
                    eth_call_meta::contract_address.eq(contract_address.as_ref()),
                    accessed_at.clone(),
                ))
                .on_conflict(eth_call_meta::contract_address)
                .do_update()
                .set(accessed_at)
                .execute(conn)
                .map(|_| ())
                .map_err(Error::from)
        })
    }
}

/// Deprecated format for the contract call id.
fn old_contract_call_id(
    contract_address: &ethabi::Address,
    encoded_call: &[u8],
    block: &EthereumBlockPointer,
) -> [u8; 16] {
    let mut id = [0; 16];
    let mut hash = tiny_keccak::Keccak::new_shake128();
    hash.update(contract_address.as_ref());
    hash.update(encoded_call);
    hash.update(block.hash.as_ref());
    hash.finalize(&mut id);
    id
}

/// The id is the hashed encoded_call + contract_address + block hash to uniquely identify the call.
/// 256 bits of output, and therefore 128 bits of security against collisions, are needed since this
/// could be targeted by a birthday attack.
fn contract_call_id(
    contract_address: &ethabi::Address,
    encoded_call: &[u8],
    block: &EthereumBlockPointer,
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(encoded_call);
    hash.update(contract_address.as_ref());
    hash.update(block.hash.as_ref());
    *hash.finalize().as_bytes()
}

/// Support for tests
#[cfg(debug_assertions)]
pub mod test_support {
    use std::str::FromStr;

    use graph::prelude::{web3::types::H256, EthereumBlockPointer};

    // Hash indicating 'no parent'
    pub const NO_PARENT: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    /// The parts of an Ethereum block that are interesting for these tests:
    /// the block number, hash, and the hash of the parent block
    #[derive(Clone, Debug, PartialEq)]
    pub struct FakeBlock {
        pub number: u64,
        pub hash: String,
        pub parent_hash: String,
    }

    impl FakeBlock {
        pub fn make_child(&self, hash: &str) -> Self {
            FakeBlock {
                number: self.number + 1,
                hash: hash.to_owned(),
                parent_hash: self.hash.clone(),
            }
        }

        pub fn make_no_parent(number: u64, hash: &str) -> Self {
            FakeBlock {
                number,
                hash: hash.to_owned(),
                parent_hash: NO_PARENT.to_string(),
            }
        }

        pub fn block_hash(&self) -> H256 {
            H256::from_str(self.hash.as_str()).expect("invalid block hash")
        }

        pub fn block_ptr(&self) -> EthereumBlockPointer {
            EthereumBlockPointer {
                number: self.number,
                hash: self.block_hash(),
            }
        }
    }

    pub type Chain = Vec<&'static FakeBlock>;

    /// Store the given chain as the blocks for the `network` set the
    /// network's genesis block to `genesis_hash`, and head block to
    /// `null`
    pub trait SettableChainStore {
        fn set_chain(&self, genesis_hash: &str, chain: Chain);
    }
}

#[cfg(debug_assertions)]
impl test_support::SettableChainStore for ChainStore {
    fn set_chain(&self, genesis_hash: &str, chain: test_support::Chain) {
        use public::ethereum_blocks as b;
        use public::ethereum_networks as n;

        let conn = self.conn.get().expect("can get a database connection");

        diesel::delete(b::table.filter(b::network_name.eq(&self.network)))
            .execute(&conn)
            .expect("Failed to delete ethereum_blocks");

        for block in &chain {
            let number = block.number as i64;

            let values = (
                b::hash.eq(&block.hash),
                b::number.eq(number),
                b::parent_hash.eq(&block.parent_hash),
                b::network_name.eq(&self.network),
                b::data.eq(serde_json::Value::Null),
            );

            insert_into(b::table)
                .values(values.clone())
                .execute(&conn)
                .unwrap();
        }

        update(n::table.filter(n::name.eq(&self.network)))
            .set((
                n::genesis_block_hash.eq(genesis_hash),
                n::head_block_hash.eq::<Option<&str>>(None),
                n::head_block_number.eq::<Option<i64>>(None),
            ))
            .execute(&conn)
            .unwrap();
    }
}
