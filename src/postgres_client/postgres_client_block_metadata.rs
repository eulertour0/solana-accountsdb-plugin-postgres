use {
    crate::{
        geyser_plugin_postgres::{GeyserPluginPostgresConfig, GeyserPluginPostgresError},
        postgres_client::{
            db_write_error, postgres_client_transaction::DbReward, SimplePostgresClient,
            UpdateBlockMetadataRequest,
        },
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPluginError, ReplicaBlockInfoV5, ReplicaBlockInfoVersions,
    },
    chrono::Utc,
    postgres::{Client, Statement},
};

#[derive(Clone, Debug)]
pub struct DbBlockInfo {
    pub slot: i64,
    pub blockhash: String,
    pub rewards: Vec<DbReward>,
    pub block_time: Option<i64>,
    pub block_height: Option<i64>,
}

impl From<&ReplicaBlockInfoV5<'_>> for DbBlockInfo {
    fn from(b: &ReplicaBlockInfoV5) -> Self {
        Self {
            slot: b.slot as i64,
            blockhash: b.blockhash.to_string(),
            rewards: b.rewards.rewards.iter().map(DbReward::from).collect(),
            block_time: b.block_time,
            block_height: b.block_height.map(|block_height| block_height as i64),
        }
    }
}

impl From<&ReplicaBlockInfoVersions<'_>> for DbBlockInfo {
    fn from(block_info: &ReplicaBlockInfoVersions) -> Self {
        match block_info {
            ReplicaBlockInfoVersions::V0_0_5(b) => Self::from(*b),
        }
    }
}

impl SimplePostgresClient {
    pub(crate) fn build_block_metadata_upsert_statement(
        client: &mut Client,
        config: &GeyserPluginPostgresConfig,
    ) -> Result<Statement, GeyserPluginError> {
        let stmt =
            "INSERT INTO block (slot, blockhash, rewards, block_time, block_height, updated_on) \
        VALUES ($1, $2, $3, $4, $5, $6) \
        ON CONFLICT (slot) DO UPDATE SET blockhash=excluded.blockhash, rewards=excluded.rewards, \
        block_time=excluded.block_time, block_height=excluded.block_height, updated_on=excluded.updated_on";

        let stmt = client.prepare(stmt);

        match stmt {
            Err(err) => {
                Err(GeyserPluginError::Custom(Box::new(GeyserPluginPostgresError::DataSchemaError {
                    msg: format!(
                        "Error in preparing for the block metadata update PostgreSQL database: ({}) host: {:?} user: {:?} config: {:?}",
                        err, config.host, config.user, config
                    ),
                })))
            }
            Ok(stmt) => Ok(stmt),
        }
    }

    pub(crate) fn update_block_metadata_impl(
        &mut self,
        block_info: UpdateBlockMetadataRequest,
    ) -> Result<(), GeyserPluginError> {
        let client = self.client.get_mut().unwrap();
        let statement = &client.update_block_metadata_stmt;
        let client = &mut client.client;
        let updated_on = Utc::now().naive_utc();

        let block_info = block_info.block_info;
        let result = client.execute(
            statement,
            &[
                &block_info.slot,
                &block_info.blockhash,
                &block_info.rewards,
                &block_info.block_time,
                &block_info.block_height,
                &updated_on,
            ],
        );

        if let Err(err) = result {
            return Err(db_write_error(
                &format!("block metadata at slot {}", block_info.slot),
                err,
            ));
        }

        Ok(())
    }
}
