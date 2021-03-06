// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    aws,
    cluster::Cluster,
    cluster_swarm::{
        cluster_swarm_kube::{ClusterSwarmKube, KubeNode},
        ClusterSwarm,
    },
    genesis_helper::GenesisHelper,
    instance::{
        fullnode_pod_name, lsr_pod_name, validator_pod_name, vault_pod_name,
        ApplicationConfig::{Fullnode, Validator, Vault, LSR},
        FullnodeConfig, Instance, InstanceConfig, LSRConfig, ValidatorConfig, ValidatorGroup,
        VaultConfig,
    },
};
use anyhow::{format_err, Result};
use futures::future::try_join_all;
use libra_logger::info;
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};
use structopt::StructOpt;

use libra_genesis_tool::layout::Layout;
use libra_global_constants::{
    CONSENSUS_KEY, EXECUTION_KEY, FULLNODE_NETWORK_KEY, LIBRA_ROOT_KEY, OPERATOR_KEY, OWNER_KEY,
    VALIDATOR_NETWORK_KEY,
};
use libra_network_address::NetworkAddress;
use libra_secure_storage::{CryptoStorage, VaultStorage};
use libra_types::chain_id::ChainId;
use std::str::FromStr;

const VAULT_TOKEN: &str = "root";
const VAULT_PORT: u32 = 8200;
const LIBRA_ROOT_NAME: &str = "libra";
const VAULT_BACKEND: &str = "vault";
const GENESIS_PATH: &str = "/tmp/genesis.blob";

#[derive(Clone, StructOpt, Debug)]
pub struct ClusterBuilderParams {
    #[structopt(long, default_value = "1")]
    pub fullnodes_per_validator: u32,
    #[structopt(long, use_delimiter = true, default_value = "")]
    cfg: Vec<String>,
    #[structopt(long, parse(try_from_str), default_value = "30")]
    pub num_validators: u32,
    #[structopt(long)]
    pub enable_lsr: Option<bool>,
    #[structopt(
        long,
        help = "Backend used by lsr. Possible Values are in-memory, on-disk, vault",
        default_value = "vault"
    )]
    pub lsr_backend: String,
}

impl ClusterBuilderParams {
    pub fn cfg_overrides(&self) -> Vec<String> {
        // Default overrides
        let mut overrides = vec!["prune_window=50000".to_string()];

        // overrides from the command line
        overrides.extend(self.cfg.iter().cloned());

        overrides
    }

    pub fn enable_lsr(&self) -> bool {
        self.enable_lsr.unwrap_or(true)
    }
}

pub struct ClusterBuilder {
    pub current_tag: String,
    pub cluster_swarm: ClusterSwarmKube,
}

impl ClusterBuilder {
    pub fn new(current_tag: String, cluster_swarm: ClusterSwarmKube) -> Self {
        Self {
            current_tag,
            cluster_swarm,
        }
    }

    pub async fn setup_cluster(
        &self,
        params: &ClusterBuilderParams,
        clean_data: bool,
    ) -> Result<Cluster> {
        self.cluster_swarm
            .cleanup()
            .await
            .map_err(|e| format_err!("cleanup on startup failed: {}", e))?;
        let current_tag = &self.current_tag;
        info!(
            "Deploying with {} tag for validators and fullnodes",
            current_tag
        );
        let asg_name = format!(
            "{}-k8s-testnet-validators",
            self.cluster_swarm
                .get_workspace()
                .await
                .expect("Failed to get workspace")
        );
        let mut instance_count =
            params.num_validators + (params.fullnodes_per_validator * params.num_validators);
        if params.enable_lsr() {
            if params.lsr_backend == "vault" {
                instance_count += params.num_validators * 2;
            } else {
                instance_count += params.num_validators;
            }
        }
        if clean_data {
            // First scale down to zero instances and wait for it to complete so that we don't schedule pods on
            // instances which are going into termination state
            aws::set_asg_size(0, 0.0, &asg_name, true, true)
                .await
                .map_err(|err| format_err!("{} scale down failed: {}", asg_name, err))?;
            // Then scale up and bring up new instances
            aws::set_asg_size(instance_count as i64, 5.0, &asg_name, true, false)
                .await
                .map_err(|err| format_err!("{} scale up failed: {}", asg_name, err))?;
        }
        let (validators, lsrs, vaults, fullnodes) = self
            .spawn_validator_and_fullnode_set(
                params.num_validators,
                params.fullnodes_per_validator,
                params.enable_lsr(),
                &params.lsr_backend,
                current_tag,
                &params.cfg_overrides(),
                clean_data,
            )
            .await
            .map_err(|e| format_err!("Failed to spawn_validator_and_fullnode_set: {}", e))?;
        let cluster = Cluster::new(validators, fullnodes, lsrs, vaults);

        info!(
            "Deployed {} validators and {} fns",
            cluster.validator_instances().len(),
            cluster.fullnode_instances().len(),
        );
        Ok(cluster)
    }

    /// Creates a set of validators and fullnodes with the given parameters
    pub async fn spawn_validator_and_fullnode_set(
        &self,
        num_validators: u32,
        num_fullnodes_per_validator: u32,
        enable_lsr: bool,
        lsr_backend: &str,
        image_tag: &str,
        config_overrides: &[String],
        clean_data: bool,
    ) -> Result<(Vec<Instance>, Vec<Instance>, Vec<Instance>, Vec<Instance>)> {
        let vault_nodes;
        let mut lsrs_nodes = vec![];
        let mut vaults = vec![];
        let mut lsrs = vec![];

        if enable_lsr {
            if lsr_backend == "vault" {
                vault_nodes = try_join_all((0..num_validators).map(|i| async move {
                    let pod_name = vault_pod_name(i);
                    self.cluster_swarm.allocate_node(&pod_name).await
                }))
                .await?;
                let mut vault_instances: Vec<_> = vault_nodes
                    .iter()
                    .enumerate()
                    .map(|(i, node)| async move {
                        let vault_config = VaultConfig {};
                        if clean_data {
                            self.cluster_swarm.clean_data(&node.name).await?;
                        }
                        self.cluster_swarm
                            .spawn_new_instance(InstanceConfig {
                                validator_group: ValidatorGroup::new_for_index(i as u32),
                                application_config: Vault(vault_config),
                            })
                            .await
                    })
                    .collect();
                vaults.append(&mut vault_instances);
            } else {
                vault_nodes = vec![];
            }
            lsrs_nodes = try_join_all((0..num_validators).map(|i| async move {
                let pod_name = lsr_pod_name(i);
                self.cluster_swarm.allocate_node(&pod_name).await
            }))
            .await?;
            let mut lsr_instances: Vec<_> = lsrs_nodes
                .iter()
                .enumerate()
                .map(|(i, node)| async move {
                    let lsr_config = LSRConfig {
                        num_validators,
                        image_tag: image_tag.to_string(),
                        lsr_backend: lsr_backend.to_string(),
                    };
                    if clean_data {
                        self.cluster_swarm.clean_data(&node.name).await?;
                    }
                    self.cluster_swarm
                        .spawn_new_instance(InstanceConfig {
                            validator_group: ValidatorGroup::new_for_index(i as u32),
                            application_config: LSR(lsr_config),
                        })
                        .await
                })
                .collect();
            lsrs.append(&mut lsr_instances);
        } else {
            vault_nodes = vec![];
        }

        let lsrs = try_join_all(lsrs).await?;
        let vaults = try_join_all(vaults).await?;

        let validator_nodes = try_join_all((0..num_validators).map(|i| async move {
            let pod_name = validator_pod_name(i);
            self.cluster_swarm.allocate_node(&pod_name).await
        }))
        .await?;

        let fullnode_nodes = try_join_all((0..num_validators).flat_map(move |validator_index| {
            (0..num_fullnodes_per_validator).map(move |fullnode_index| async move {
                let pod_name = fullnode_pod_name(validator_index, fullnode_index);
                self.cluster_swarm.allocate_node(&pod_name).await
            })
        }))
        .await?;

        if !vault_nodes.is_empty() {
            info!("Generating genesis with management tool.");
            try_join_all(vault_nodes.iter().enumerate().map(|(i, node)| async move {
                libra_retrier::retry_async(libra_retrier::fixed_retry_strategy(5000, 15), || {
                    Box::pin(async move { self.initialize_vault(i as u32, node).await })
                })
                .await
            }))
            .await?;

            self.generate_genesis(
                num_validators,
                &vault_nodes,
                &validator_nodes,
                &fullnode_nodes,
            )
            .await?;
            info!("Done generating genesis.");
        }

        let validators = (0..num_validators).map(|i| {
            let validator_nodes = &validator_nodes;
            let lsrs_nodes = &lsrs_nodes;
            async move {
                let seed_peer_ip = validator_nodes[0].internal_ip.clone();
                let safety_rules_addr = if enable_lsr {
                    Some(lsrs_nodes[i as usize].internal_ip.clone())
                } else {
                    None
                };
                let validator_config = ValidatorConfig {
                    num_validators,
                    num_fullnodes: num_fullnodes_per_validator,
                    enable_lsr,
                    image_tag: image_tag.to_string(),
                    config_overrides: config_overrides.to_vec(),
                    seed_peer_ip,
                    safety_rules_addr,
                };
                if clean_data {
                    self.cluster_swarm
                        .clean_data(&validator_nodes[i as usize].name)
                        .await?;
                }
                self.cluster_swarm
                    .spawn_new_instance(InstanceConfig {
                        validator_group: ValidatorGroup::new_for_index(i),
                        application_config: Validator(validator_config),
                    })
                    .await
            }
        });

        let fullnodes = (0..num_validators).flat_map(|validator_index| {
            let fullnode_nodes = &fullnode_nodes;
            let validator_nodes = &validator_nodes;
            (0..num_fullnodes_per_validator).map(move |fullnode_index| async move {
                let seed_peer_ip = validator_nodes[validator_index as usize]
                    .internal_ip
                    .clone();
                let fullnode_config = FullnodeConfig {
                    fullnode_index,
                    num_fullnodes_per_validator,
                    num_validators,
                    image_tag: image_tag.to_string(),
                    config_overrides: config_overrides.to_vec(),
                    seed_peer_ip,
                };
                if clean_data {
                    self.cluster_swarm
                        .clean_data(
                            &fullnode_nodes[(validator_index * num_fullnodes_per_validator
                                + fullnode_index)
                                as usize]
                                .name,
                        )
                        .await?;
                }
                self.cluster_swarm
                    .spawn_new_instance(InstanceConfig {
                        validator_group: ValidatorGroup::new_for_index(validator_index),
                        application_config: Fullnode(fullnode_config),
                    })
                    .await
            })
        });

        let validators = try_join_all(validators).await?;
        let fullnodes = try_join_all(fullnodes).await?;
        Ok((validators, lsrs, vaults, fullnodes))
    }

    async fn initialize_vault(&self, validator_index: u32, vault_node: &KubeNode) -> Result<()> {
        let addr = vault_node.internal_ip.clone();
        tokio::task::spawn_blocking(move || {
            let mut vault_storage = Box::new(VaultStorage::new(
                format!("http://{}:{}", addr, VAULT_PORT),
                VAULT_TOKEN.to_string(),
                None,
                None,
            ));
            if validator_index == 0 {
                let libra_root_key = format!("{}__{}", LIBRA_ROOT_NAME, LIBRA_ROOT_KEY);
                vault_storage
                    .create_key(&libra_root_key)
                    .map_err(|e| format_err!("Failed to create {} : {}", libra_root_key, e))?;
            }
            let pod_name = validator_pod_name(validator_index);
            let keys = vec![
                OWNER_KEY,
                OPERATOR_KEY,
                CONSENSUS_KEY,
                EXECUTION_KEY,
                VALIDATOR_NETWORK_KEY,
                FULLNODE_NETWORK_KEY,
            ];
            for key in keys {
                let key = format!("{}__{}", pod_name, key);
                vault_storage
                    .create_key(&key)
                    .map_err(|e| format_err!("Failed to create {} : {}", key, e))?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    async fn generate_genesis(
        &self,
        num_validators: u32,
        vault_nodes: &[KubeNode],
        validator_nodes: &[KubeNode],
        fullnode_nodes: &[KubeNode],
    ) -> Result<()> {
        let genesis_helper = GenesisHelper::new("/tmp/genesis.json");
        let owners: Vec<_> = (0..num_validators).map(validator_pod_name).collect();
        let layout = Layout {
            owners: owners.clone(),
            operators: owners,
            libra_root: vec![LIBRA_ROOT_NAME.to_string()],
        };
        let layout_path = "/tmp/layout.yaml";
        write!(
            File::create(layout_path).map_err(|e| format_err!(
                "Failed to create {} : {}",
                layout_path,
                e
            ))?,
            "{}",
            toml::to_string(&layout)?
        )
        .map_err(|e| format_err!("Failed to write {} : {}", layout_path, e))?;
        let token_path = "/tmp/token";
        write!(
            File::create(token_path).map_err(|e| format_err!(
                "Failed to create {} : {}",
                token_path,
                e
            ))?,
            "{}",
            VAULT_TOKEN
        )
        .map_err(|e| format_err!("Failed to write {} : {}", token_path, e))?;
        genesis_helper
            .set_layout(layout_path, "common")
            .await
            .map_err(|e| format_err!("Failed to set_layout : {}", e))?;
        genesis_helper
            .libra_root_key(
                VAULT_BACKEND,
                format!("http://{}:{}", vault_nodes[0].internal_ip, VAULT_PORT).as_str(),
                token_path,
                LIBRA_ROOT_NAME,
                LIBRA_ROOT_NAME,
            )
            .await
            .map_err(|e| format_err!("Failed to libra_root_key : {}", e))?;

        for (i, node) in vault_nodes.iter().enumerate() {
            let pod_name = validator_pod_name(i as u32);
            genesis_helper
                .owner_key(
                    VAULT_BACKEND,
                    format!("http://{}:{}", node.internal_ip, VAULT_PORT).as_str(),
                    token_path,
                    &pod_name,
                    &pod_name,
                )
                .await
                .map_err(|e| format_err!("Failed to owner_key for {} : {}", pod_name, e))?;
            genesis_helper
                .operator_key(
                    VAULT_BACKEND,
                    format!("http://{}:{}", node.internal_ip, VAULT_PORT).as_str(),
                    token_path,
                    &pod_name,
                    &pod_name,
                )
                .await
                .map_err(|e| format_err!("Failed to operator_key for {} : {}", pod_name, e))?;
            genesis_helper
                .validator_config(
                    &pod_name,
                    NetworkAddress::from_str(
                        format!("/ip4/{}/tcp/{}", validator_nodes[i].internal_ip, 6180).as_str(),
                    )
                    .expect("Failed to parse network address"),
                    NetworkAddress::from_str(
                        format!("/ip4/{}/tcp/{}", fullnode_nodes[i].internal_ip, 6180).as_str(),
                    )
                    .expect("Failed to parse network address"),
                    ChainId::new(1),
                    VAULT_BACKEND,
                    format!("http://{}:{}", node.internal_ip, VAULT_PORT).as_str(),
                    token_path,
                    &pod_name,
                    &pod_name,
                )
                .await
                .map_err(|e| format_err!("Failed to validator_config for {} : {}", pod_name, e))?;
            genesis_helper
                .set_operator(&pod_name, &pod_name)
                .await
                .map_err(|e| format_err!("Failed to set_operator for {} : {}", pod_name, e))?;
        }
        genesis_helper
            .genesis(ChainId::new(1), Path::new(GENESIS_PATH))
            .await?;
        for (i, node) in vault_nodes.iter().enumerate() {
            let pod_name = validator_pod_name(i as u32);
            genesis_helper
                .create_and_insert_waypoint(
                    ChainId::new(1),
                    VAULT_BACKEND,
                    format!("http://{}:{}", node.internal_ip, VAULT_PORT).as_str(),
                    token_path,
                    &pod_name,
                )
                .await
                .map_err(|e| {
                    format_err!(
                        "Failed to create_and_insert_waypoint for {} : {}",
                        pod_name,
                        e
                    )
                })?;
        }
        genesis_helper
            .extract_private_key(
                format!("{}__{}", LIBRA_ROOT_NAME, LIBRA_ROOT_KEY).as_str(),
                "/tmp/mint.key",
                VAULT_BACKEND,
                format!("http://{}:{}", vault_nodes[0].internal_ip, VAULT_PORT).as_str(),
                token_path,
            )
            .await
            .map_err(|e| format_err!("Failed to extract_private_key : {}", e))?;

        try_join_all(
            validator_nodes
                .iter()
                .enumerate()
                .map(|(i, node)| async move {
                    let genesis = fs::read(GENESIS_PATH)
                        .map_err(|e| format_err!("Failed to read {} : {}", GENESIS_PATH, e))?;
                    self.cluster_swarm
                        .put_file(
                            &node.name,
                            validator_pod_name(i as u32).as_str(),
                            "/opt/libra/etc/genesis2.blob",
                            genesis,
                        )
                        .await
                }),
        )
        .await
        .map_err(|e| format_err!("Failed to copy genesis.blob to validator nodes : {}", e))?;

        Ok(())
    }
}
