//! Application client for data fetching and reconstruction.
//!
//! App client is enabled when app_id is configured and greater than 0 in avail-light configuration. [`Light client`](super::light_client) triggers application client if block is verified with high enough confidence. Currently [`run`] function is separate task and doesn't block main thread.
//!
//! # Flow
//!
//! Get app data rows from node
//! Verify commitment equality for each row
//! Decode app data and store it into local database under the `app_id:block_number` key
//!
//! # Notes
//!
//! If application client fails to run or stops its execution, error is logged, and other tasks continue with execution.

use anyhow::{anyhow, Context, Result};
use avail_subxt::AvailConfig;
use dusk_plonk::commitment_scheme::kzg10::PublicParameters;
use kate_recovery::{
	com::{app_specific_rows, columns_positions, decode_app_extrinsics, reconstruct_columns},
	commitments,
	config::{self, CHUNK_SIZE},
	data::{Cell, DataCell},
	matrix::{Dimensions, Position},
};
use rocksdb::DB;
use std::{
	collections::{HashMap, HashSet},
	sync::{mpsc::Receiver, Arc},
};
use subxt::OnlineClient;
use tracing::{debug, error, info, instrument};

use crate::{
	data::store_encoded_data_in_db,
	network::Client,
	proof, rpc,
	types::{AppClientConfig, BlockVerified},
};

fn new_data_cell(row: usize, col: usize, data: &[u8]) -> Result<DataCell> {
	Ok(DataCell {
		position: Position {
			row: row.try_into()?,
			col: col.try_into()?,
		},
		data: data.try_into()?,
	})
}

fn data_cells_from_row(row: usize, row_data: &[u8]) -> Result<Vec<DataCell>> {
	row_data
		.chunks_exact(CHUNK_SIZE)
		.enumerate()
		.map(move |(col, data)| new_data_cell(row, col, data))
		.collect::<Result<Vec<DataCell>>>()
}

fn data_cells_from_rows(rows: Vec<Option<Vec<u8>>>) -> Result<Vec<DataCell>> {
	Ok(rows
		.into_iter()
		.enumerate() // Add row indexes
		.filter_map(|(row, row_data)| row_data.map(|data| (row, data))) // Remove None rows
		.map(|(row, data)| data_cells_from_row(row, &data))
		.collect::<Result<Vec<Vec<DataCell>>, _>>()?
		.into_iter()
		.flatten()
		.collect::<Vec<_>>())
}

fn data_cell(
	position: Position,
	reconstructed: &HashMap<u16, Vec<[u8; config::CHUNK_SIZE]>>,
) -> Result<DataCell> {
	let row: usize = position.row.try_into()?;
	reconstructed
		.get(&position.col)
		// Dividing with extension factor since reconstracted column is not extended
		.and_then(|column| column.get(row / config::EXTENSION_FACTOR))
		.map(|&data| DataCell { position, data })
		.context("Data cell not found")
}

async fn fetch_verified(
	pp: &PublicParameters,
	network_client: &Client,
	block_number: u32,
	dimensions: &Dimensions,
	commitments: &[[u8; config::COMMITMENT_SIZE]],
	positions: &[Position],
) -> Result<(Vec<Cell>, Vec<Position>)> {
	let (mut fetched, mut unfetched) = network_client
		.fetch_cells_from_dht(block_number, positions)
		.await;

	let (verified, mut unverified) =
		proof::verify(block_number, dimensions, &fetched, commitments, pp)
			.context("Failed to verify fetched cells")?;

	fetched.retain(|cell| verified.contains(&cell.position));
	unfetched.append(&mut unverified);

	Ok((fetched, unfetched))
}

async fn reconstruct_rows_from_dht(
	pp: PublicParameters,
	network_client: Client,
	block_number: u32,
	dimensions: &Dimensions,
	commitments: &[[u8; config::COMMITMENT_SIZE]],
	missing_rows: &[u32],
) -> Result<Vec<(u32, Vec<u8>)>> {
	let missing_cells = dimensions.extended_rows_positions(missing_rows);

	if missing_cells.is_empty() {
		return Ok(vec![]);
	}

	debug!(
		block_number,
		"Fetching {} missing row cells from DHT",
		missing_cells.len()
	);
	let (fetched, unfetched) = fetch_verified(
		&pp,
		&network_client,
		block_number,
		dimensions,
		commitments,
		&missing_cells,
	)
	.await?;
	debug!(
		block_number,
		"Fetched {} row cells, {} row cells is missing",
		fetched.len(),
		unfetched.len()
	);

	let missing_cells = columns_positions(dimensions, &unfetched, 0.66);

	let (missing_fetched, _) = fetch_verified(
		&pp,
		&network_client,
		block_number,
		dimensions,
		commitments,
		&missing_cells,
	)
	.await?;

	let reconstructed = reconstruct_columns(dimensions, &missing_fetched)?;

	debug!(
		block_number,
		"Reconstructed {} columns: {:?}",
		reconstructed.keys().len(),
		reconstructed.keys()
	);

	let mut reconstructed_cells = unfetched
		.into_iter()
		.map(|position| data_cell(position, &reconstructed))
		.collect::<Result<Vec<_>>>()?;

	debug!(
		block_number,
		"Reconstructed {} missing row cells",
		reconstructed_cells.len()
	);

	let mut data_cells: Vec<DataCell> = fetched.into_iter().map(Into::into).collect::<Vec<_>>();

	data_cells.append(&mut reconstructed_cells);

	data_cells
		.sort_by(|a, b| (a.position.row, a.position.col).cmp(&(b.position.row, b.position.col)));

	missing_rows
		.iter()
		.map(|&row| {
			let data = data_cells
				.iter()
				.filter(|&cell| cell.position.row == row)
				.flat_map(|cell| cell.data)
				.collect::<Vec<_>>();

			if data.len() != dimensions.cols() as usize * config::CHUNK_SIZE {
				return Err(anyhow!("Row size is not valid after reconstruction"));
			}

			Ok((row, data))
		})
		.collect::<Result<Vec<_>>>()
}

#[instrument(skip_all, fields(block = block.block_num), level = "trace")]
async fn process_block(
	cfg: &AppClientConfig,
	db: Arc<DB>,
	network_client: Client,
	rpc_client: &OnlineClient<AvailConfig>,
	app_id: u32,
	block: &BlockVerified,
	pp: PublicParameters,
) -> Result<()> {
	let lookup = &block.lookup;
	let block_number = block.block_num;
	let dimensions = &block.dimensions;

	let commitments = &block.commitments;

	let app_rows = app_specific_rows(lookup, dimensions, app_id);

	debug!(
		block_number,
		"Fetching {} app rows from DHT: {app_rows:?}",
		app_rows.len()
	);

	let dht_rows = network_client
		.fetch_rows_from_dht(block_number, dimensions, &app_rows)
		.await;

	let dht_rows_count = dht_rows.iter().flatten().count();
	debug!(block_number, "Fetched {dht_rows_count} app rows from DHT");

	let (dht_verified_rows, dht_missing_rows) =
		commitments::verify_equality(&pp, commitments, &dht_rows, lookup, dimensions, app_id)?;
	debug!(
		block_number,
		"Verified {} app rows from DHT, missing {}",
		dht_verified_rows.len(),
		dht_missing_rows.len()
	);

	let rpc_rows = if cfg.disable_rpc {
		vec![None; dht_rows.len()]
	} else {
		debug!(
			block_number,
			"Fetching missing app rows from RPC: {dht_missing_rows:?}",
		);
		rpc::get_kate_rows(rpc_client, dht_missing_rows, block.header_hash).await?
	};

	let (rpc_verified_rows, mut missing_rows) =
		commitments::verify_equality(&pp, commitments, &rpc_rows, lookup, dimensions, app_id)?;
	// Since verify_equality returns all missing rows, exclude DHT rows that are already verified
	missing_rows.retain(|row| !dht_verified_rows.contains(row));

	debug!(
		block_number,
		"Verified {} app rows from RPC, missing {}",
		rpc_verified_rows.len(),
		missing_rows.len()
	);

	let verified_rows_iter = dht_verified_rows
		.into_iter()
		.chain(rpc_verified_rows.into_iter());
	let verified_rows: HashSet<u32> = HashSet::from_iter(verified_rows_iter);

	let mut rows = dht_rows
		.into_iter()
		.zip(rpc_rows.into_iter())
		.zip(0..dimensions.extended_rows())
		.map(|((dht_row, rpc_row), row_index)| {
			let row = dht_row.or(rpc_row)?;
			verified_rows.contains(&row_index).then_some(row)
		})
		.collect::<Vec<_>>();

	let rows_count = rows.iter().filter(|row| row.is_some()).count();
	debug!(
		block_number,
		"Found {rows_count} rows, verified {}, {} is missing",
		verified_rows.len(),
		missing_rows.len()
	);

	if missing_rows.len() * dimensions.cols() as usize > cfg.threshold {
		return Err(anyhow::anyhow!("Too many cells are missing"));
	}

	debug!(
		block_number,
		"Reconstructing {} missing app rows from DHT: {missing_rows:?}",
		missing_rows.len()
	);
	let dht_rows = reconstruct_rows_from_dht(
		pp,
		network_client,
		block_number,
		dimensions,
		commitments,
		&missing_rows,
	)
	.await?;

	debug!(
		block_number,
		"Reconstructed {} app rows from DHT",
		dht_rows.len()
	);

	for (row_index, row) in dht_rows {
		let i: usize = row_index.try_into()?;
		rows[i] = Some(row);
	}

	let data_cells =
		data_cells_from_rows(rows).context("Failed to create data cells from rows got from RPC")?;

	let data = decode_app_extrinsics(lookup, dimensions, data_cells, app_id)
		.context("Failed to decode app extrinsics")?;

	debug!(block_number, "Storing data into database");
	store_encoded_data_in_db(db, app_id, block_number, &data)
		.context("Failed to store data into database")?;

	let bytes_count = data.iter().fold(0usize, |acc, x| acc + x.len());
	debug!(block_number, "Stored {bytes_count} bytes into database");

	Ok(())
}

/// Runs application client.
///
/// # Arguments
///
/// * `cfg` - Application client configuration
/// * `db` - Database to store data inot DB
/// * `network_client` - Reference to a libp2p custom network client
/// * `rpc_client` - Node's RPC subxt client for fetching data unavailable in DHT (if configured)
/// * `app_id` - Application ID
/// * `block_receive` - Channel used to receive header of verified block
/// * `pp` - Public parameters (i.e. SRS) needed for proof verification
pub async fn run(
	cfg: AppClientConfig,
	db: Arc<DB>,
	network_client: Client,
	rpc_client: OnlineClient<AvailConfig>,
	app_id: u32,
	block_receive: Receiver<BlockVerified>,
	pp: PublicParameters,
) {
	info!("Starting for app {app_id}...");

	for block in block_receive {
		let block_number = block.block_num;
		let dimensions = &block.dimensions;

		info!(block_number, "Block available: {dimensions:?}");

		if block.dimensions.cols() == 0 {
			info!(block_number, "Skipping empty block");
			continue;
		}

		if block
			.lookup
			.index
			.iter()
			.filter(|&(id, _)| id == &app_id)
			.count() == 0
		{
			info!(
				block_number,
				"Skipping block with no cells for app {app_id}"
			);
			continue;
		}

		if let Err(error) = process_block(
			&cfg,
			db.clone(),
			network_client.clone(),
			&rpc_client,
			app_id,
			&block,
			pp.clone(),
		)
		.await
		{
			error!(block_number, "Cannot process block: {error}");
		} else {
			debug!(block_number, "Block processed");
		}
	}
}
