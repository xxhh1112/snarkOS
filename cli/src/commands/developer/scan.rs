// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(clippy::type_complexity)]

use super::CurrentNetwork;

use snarkvm::prelude::{block::Block, Ciphertext, Field, Network, Plaintext, PrivateKey, Record, ViewKey};

use anyhow::{bail, ensure, Result};
use clap::Parser;
use parking_lot::RwLock;
use std::{
    io::{stdout, Write},
    str::FromStr,
    sync::Arc,
};

const MAX_BLOCK_RANGE: u32 = 50;

/// Scan the snarkOS node for records.
#[derive(Debug, Parser)]
pub struct Scan {
    /// An optional private key scan for unspent records.
    #[clap(short, long)]
    private_key: Option<String>,

    /// The view key used to scan for records.
    #[clap(short, long)]
    view_key: Option<String>,

    /// The block height to start scanning from.
    #[clap(long, conflicts_with = "last")]
    start: Option<u32>,

    /// The block height to stop scanning.
    #[clap(long, conflicts_with = "last")]
    end: Option<u32>,

    /// Scan the latest `n` blocks.
    #[clap(long)]
    last: Option<u32>,

    /// The endpoint to scan blocks from.
    #[clap(long)]
    endpoint: String,

    /// Enables the node to prefetch initial blocks from a CDN
    #[clap(default_value = "https://s3.us-west-1.amazonaws.com/testnet3.blocks/phase3", long = "cdn")]
    cdn: String,
}

impl Scan {
    pub fn parse(self) -> Result<String> {
        // Derive the view key and optional private key.
        let (private_key, view_key) = self.parse_account()?;

        // Find the start and end height to scan.
        let (start_height, end_height) = self.parse_block_range()?;

        // Fetch the records from the network.
        let records = Self::fetch_records(private_key, &view_key, &self.cdn, &self.endpoint, start_height, end_height)?;

        // Output the decrypted records associated with the view key.
        if records.is_empty() {
            Ok("No records found".to_string())
        } else {
            if private_key.is_none() {
                println!("⚠️  This list may contain records that have already been spent.\n");
            }

            Ok(serde_json::to_string_pretty(&records)?.replace("\\n", ""))
        }
    }

    /// Returns the view key and optional private key, from the given configurations.
    fn parse_account<N: Network>(&self) -> Result<(Option<PrivateKey<N>>, ViewKey<N>)> {
        match (&self.private_key, &self.view_key) {
            (Some(private_key), Some(view_key)) => {
                // Derive the private key.
                let private_key = PrivateKey::<N>::from_str(private_key)?;
                // Derive the expected view key.
                let expected_view_key = ViewKey::<N>::try_from(private_key)?;
                // Derive the view key.
                let view_key = ViewKey::<N>::from_str(view_key)?;

                ensure!(
                    expected_view_key == view_key,
                    "The provided private key does not correspond to the provided view key."
                );

                Ok((Some(private_key), view_key))
            }
            (Some(private_key), _) => {
                // Derive the private key.
                let private_key = PrivateKey::<N>::from_str(private_key)?;
                // Derive the view key.
                let view_key = ViewKey::<N>::try_from(private_key)?;

                Ok((Some(private_key), view_key))
            }
            (None, Some(view_key)) => Ok((None, ViewKey::<N>::from_str(view_key)?)),
            (None, None) => bail!("Missing private key or view key."),
        }
    }

    /// Returns the `start` and `end` blocks to scan.
    fn parse_block_range(&self) -> Result<(u32, u32)> {
        match (self.start, self.end, self.last) {
            (Some(start), Some(end), None) => {
                ensure!(end > start, "The given scan range is invalid (start = {start}, end = {end})");

                Ok((start, end))
            }
            (Some(start), None, None) => {
                // Request the latest block height from the endpoint.
                let endpoint = format!("{}/testnet3/latest/height", self.endpoint);
                let latest_height = u32::from_str(&ureq::get(&endpoint).call()?.into_string()?)?;

                // Print warning message if the user is attempting to scan the whole chain.
                if start == 0 {
                    println!("⚠️  Attention - Scanning the entire chain. This may take a while...\n");
                }

                Ok((start, latest_height))
            }
            (None, Some(end), None) => Ok((0, end)),
            (None, None, Some(last)) => {
                // Request the latest block height from the endpoint.
                let endpoint = format!("{}/testnet3/latest/height", self.endpoint);
                let latest_height = u32::from_str(&ureq::get(&endpoint).call()?.into_string()?)?;

                Ok((latest_height.saturating_sub(last), latest_height))
            }
            (None, None, None) => bail!("Missing data about block range."),
            _ => bail!("`last` flags can't be used with `start` or `end`"),
        }
    }

    /// Fetch owned ciphertext records from the endpoint.
    fn fetch_records(
        private_key: Option<PrivateKey<CurrentNetwork>>,
        view_key: &ViewKey<CurrentNetwork>,
        cdn: &str,
        endpoint: &str,
        start_height: u32,
        end_height: u32,
    ) -> Result<Vec<Record<CurrentNetwork, Plaintext<CurrentNetwork>>>> {
        // Check the bounds of the request.
        if start_height > end_height {
            bail!("Invalid block range");
        }

        // Derive the x-coordinate of the address corresponding to the given view key.
        let address_x_coordinate = view_key.to_address().to_x_coordinate();

        // Initialize a vector to store the records.
        let records = Arc::new(RwLock::new(Vec::new()));

        // Calculate the number of blocks to scan.
        let total_blocks = end_height.saturating_sub(start_height);

        // Log the initial progress.
        print!("\rScanning {total_blocks} blocks for records (0% complete)...");
        stdout().flush()?;

        // Scan the CDN first for records.
        Self::scan_from_cdn(
            start_height,
            end_height,
            cdn.to_string(),
            endpoint.to_string(),
            private_key,
            *view_key,
            address_x_coordinate,
            records.clone(),
        )?;

        // Scan the endpoint for the remaining blocks.
        let mut request_start = end_height.saturating_sub(start_height % MAX_BLOCK_RANGE);
        while request_start <= end_height {
            // Log the progress.
            let percentage_complete = request_start.saturating_sub(start_height) as f64 * 100.0 / total_blocks as f64;
            print!("\rScanning {total_blocks} blocks for records ({percentage_complete:.2}% complete)...");
            stdout().flush()?;

            let num_blocks_to_request =
                std::cmp::min(MAX_BLOCK_RANGE, end_height.saturating_sub(request_start).saturating_add(1));
            let request_end = request_start.saturating_add(num_blocks_to_request);

            // Establish the endpoint.
            let blocks_endpoint = format!("{endpoint}/testnet3/blocks?start={request_start}&end={request_end}");
            // Fetch blocks
            let blocks: Vec<Block<CurrentNetwork>> = ureq::get(&blocks_endpoint).call()?.into_json()?;

            // Scan the blocks for owned records.
            for block in &blocks {
                Self::scan_block(block, endpoint, private_key, view_key, &address_x_coordinate, records.clone())?;
            }

            request_start = request_start.saturating_add(num_blocks_to_request);
        }

        // Print final complete message.
        println!("\rScanning {total_blocks} blocks for records (100% complete)...   \n");
        stdout().flush()?;

        let result = records.read().clone();
        Ok(result)
    }

    /// Scan the blocks from the CDN.
    #[allow(clippy::too_many_arguments)]
    fn scan_from_cdn(
        start_height: u32,
        end_height: u32,
        cdn: String,
        endpoint: String,
        private_key: Option<PrivateKey<CurrentNetwork>>,
        view_key: ViewKey<CurrentNetwork>,
        address_x_coordinate: Field<CurrentNetwork>,
        records: Arc<RwLock<Vec<Record<CurrentNetwork, Plaintext<CurrentNetwork>>>>>,
    ) -> Result<()> {
        // Calculate the number of blocks to scan.
        let total_blocks = end_height.saturating_sub(start_height);

        // Get the start_height with
        let cdn_request_start = start_height.saturating_sub(start_height % MAX_BLOCK_RANGE);
        let cdn_request_end = end_height.saturating_sub(start_height % MAX_BLOCK_RANGE);

        // Construct the runtime.
        let rt = tokio::runtime::Runtime::new()?;

        // Scan the blocks via the CDN.
        rt.block_on(async move {
            let _ = snarkos_node_cdn::load_blocks(&cdn, cdn_request_start, Some(cdn_request_end), move |block| {
                // Check if the block is within the requested range.
                if block.height() < start_height || block.height() > end_height {
                    return Ok(());
                }

                // Log the progress.
                let percentage_complete =
                    block.height().saturating_sub(start_height) as f64 * 100.0 / total_blocks as f64;
                print!("\rScanning {total_blocks} blocks for records ({percentage_complete:.2}% complete)...");
                stdout().flush()?;

                // Scan the block for records.
                Self::scan_block(&block, &endpoint, private_key, &view_key, &address_x_coordinate, records.clone())?;

                Ok(())
            })
            .await;
        });

        Ok(())
    }

    /// Scan a block for owned records.
    fn scan_block(
        block: &Block<CurrentNetwork>,
        endpoint: &str,
        private_key: Option<PrivateKey<CurrentNetwork>>,
        view_key: &ViewKey<CurrentNetwork>,
        address_x_coordinate: &Field<CurrentNetwork>,
        records: Arc<RwLock<Vec<Record<CurrentNetwork, Plaintext<CurrentNetwork>>>>>,
    ) -> Result<()> {
        for (commitment, ciphertext_record) in block.records() {
            // Check if the record is owned by the given view key.
            if ciphertext_record.is_owner_with_address_x_coordinate(view_key, address_x_coordinate) {
                // Decrypt and optionally filter the records.
                if let Some(record) =
                    Self::decrypt_record(private_key, view_key, endpoint, *commitment, ciphertext_record)?
                {
                    records.write().push(record);
                }
            }
        }

        Ok(())
    }

    /// Decrypts the ciphertext record and filters spend record if a private key was provided.
    fn decrypt_record(
        private_key: Option<PrivateKey<CurrentNetwork>>,
        view_key: &ViewKey<CurrentNetwork>,
        endpoint: &str,
        commitment: Field<CurrentNetwork>,
        ciphertext_record: &Record<CurrentNetwork, Ciphertext<CurrentNetwork>>,
    ) -> Result<Option<Record<CurrentNetwork, Plaintext<CurrentNetwork>>>> {
        // Check if a private key was provided.
        if let Some(private_key) = private_key {
            // Compute the serial number.
            let serial_number =
                Record::<CurrentNetwork, Plaintext<CurrentNetwork>>::serial_number(private_key, commitment)?;

            // Establish the endpoint.
            let endpoint = format!("{endpoint}/testnet3/find/transitionID/{serial_number}");

            // Check if the record is spent.
            match ureq::get(&endpoint).call() {
                // On success, skip as the record is spent.
                Ok(_) => Ok(None),
                // On error, add the record.
                Err(_error) => {
                    // TODO: Dedup the error types. We're adding the record as valid because the endpoint failed,
                    //  meaning it couldn't find the serial number (ie. unspent). However if there's a DNS error or request error,
                    //  we have a false positive here then.
                    // Decrypt the record.
                    Ok(Some(ciphertext_record.decrypt(view_key)?))
                }
            }
        } else {
            // If no private key was provided, return the record.
            Ok(Some(ciphertext_record.decrypt(view_key)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snarkvm::prelude::{TestRng, Testnet3};

    type CurrentNetwork = Testnet3;

    #[test]
    fn test_parse_account() {
        let rng = &mut TestRng::default();

        // Generate private key and view key.
        let private_key = PrivateKey::<CurrentNetwork>::new(rng).unwrap();
        let view_key = ViewKey::try_from(private_key).unwrap();

        // Generate unassociated private key and view key.
        let unassociated_private_key = PrivateKey::<CurrentNetwork>::new(rng).unwrap();
        let unassociated_view_key = ViewKey::try_from(unassociated_private_key).unwrap();

        let config = Scan::try_parse_from(
            [
                "snarkos",
                "--private-key",
                &format!("{private_key}"),
                "--view-key",
                &format!("{view_key}"),
                "--last",
                "10",
                "--endpoint",
                "",
            ]
            .iter(),
        )
        .unwrap();
        assert!(config.parse_account::<CurrentNetwork>().is_ok());

        let config = Scan::try_parse_from(
            [
                "snarkos",
                "--private-key",
                &format!("{private_key}"),
                "--view-key",
                &format!("{unassociated_view_key}"),
                "--last",
                "10",
                "--endpoint",
                "",
            ]
            .iter(),
        )
        .unwrap();
        assert!(config.parse_account::<CurrentNetwork>().is_err());
    }

    #[test]
    fn test_parse_block_range() {
        let config =
            Scan::try_parse_from(["snarkos", "--view-key", "", "--start", "0", "--end", "10", "--endpoint", ""].iter())
                .unwrap();
        assert!(config.parse_block_range().is_ok());

        // `start` height can't be greater than `end` height.
        let config =
            Scan::try_parse_from(["snarkos", "--view-key", "", "--start", "10", "--end", "5", "--endpoint", ""].iter())
                .unwrap();
        assert!(config.parse_block_range().is_err());

        // `last` conflicts with `start`
        assert!(
            Scan::try_parse_from(
                ["snarkos", "--view-key", "", "--start", "0", "--last", "10", "--endpoint", ""].iter(),
            )
            .is_err()
        );

        // `last` conflicts with `end`
        assert!(
            Scan::try_parse_from(["snarkos", "--view-key", "", "--end", "10", "--last", "10", "--endpoint", ""].iter())
                .is_err()
        );

        // `last` conflicts with `start` and `end`
        assert!(
            Scan::try_parse_from(
                ["snarkos", "--view-key", "", "--start", "0", "--end", "01", "--last", "10", "--endpoint", ""].iter(),
            )
            .is_err()
        );
    }
}
