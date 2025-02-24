use crate::{
    apis::{
        coredb_types::{CoreDB, S3Credentials},
        postgres_parameters::MergeError,
    },
    cloudnativepg::{
        backups::Backup,
        clusters::{
            Cluster, ClusterAffinity, ClusterBackup, ClusterBackupBarmanObjectStore,
            ClusterBackupBarmanObjectStoreData, ClusterBackupBarmanObjectStoreDataCompression,
            ClusterBackupBarmanObjectStoreDataEncryption,
            ClusterBackupBarmanObjectStoreS3Credentials,
            ClusterBackupBarmanObjectStoreS3CredentialsAccessKeyId,
            ClusterBackupBarmanObjectStoreS3CredentialsRegion,
            ClusterBackupBarmanObjectStoreS3CredentialsSecretAccessKey,
            ClusterBackupBarmanObjectStoreS3CredentialsSessionToken,
            ClusterBackupBarmanObjectStoreWal, ClusterBackupBarmanObjectStoreWalCompression,
            ClusterBackupBarmanObjectStoreWalEncryption, ClusterBootstrap, ClusterBootstrapInitdb,
            ClusterBootstrapRecovery, ClusterBootstrapRecoveryRecoveryTarget, ClusterCertificates,
            ClusterExternalClusters, ClusterExternalClustersBarmanObjectStore,
            ClusterExternalClustersBarmanObjectStoreS3Credentials,
            ClusterExternalClustersBarmanObjectStoreS3CredentialsAccessKeyId,
            ClusterExternalClustersBarmanObjectStoreS3CredentialsRegion,
            ClusterExternalClustersBarmanObjectStoreS3CredentialsSecretAccessKey,
            ClusterExternalClustersBarmanObjectStoreS3CredentialsSessionToken,
            ClusterExternalClustersBarmanObjectStoreWal,
            ClusterExternalClustersBarmanObjectStoreWalCompression,
            ClusterExternalClustersBarmanObjectStoreWalEncryption, ClusterExternalClustersPassword,
            ClusterLogLevel, ClusterManaged, ClusterManagedRoles, ClusterManagedRolesEnsure,
            ClusterManagedRolesPasswordSecret, ClusterMonitoring,
            ClusterMonitoringCustomQueriesConfigMap, ClusterNodeMaintenanceWindow,
            ClusterPostgresql, ClusterPostgresqlSyncReplicaElectionConstraint,
            ClusterPrimaryUpdateMethod, ClusterPrimaryUpdateStrategy, ClusterReplicationSlots,
            ClusterReplicationSlotsHighAvailability, ClusterResources,
            ClusterServiceAccountTemplate, ClusterServiceAccountTemplateMetadata, ClusterSpec,
            ClusterStorage, ClusterSuperuserSecret,
        },
        poolers::{
            Pooler, PoolerCluster, PoolerPgbouncer, PoolerSpec, PoolerTemplate, PoolerTemplateSpec,
            PoolerTemplateSpecContainers, PoolerType,
        },
        scheduledbackups::{
            ScheduledBackup, ScheduledBackupBackupOwnerReference, ScheduledBackupCluster,
            ScheduledBackupSpec,
        },
    },
    config::Config,
    configmap::custom_metrics_configmap_settings,
    defaults::{default_dw_image_uri, default_image_uri, default_llm_image_uri},
    errors::ValueError,
    is_postgres_ready, patch_cdb_status_merge,
    postgres_exporter::EXPORTER_CONFIGMAP_PREFIX,
    psql::PsqlOutput,
    trunk::extensions_that_require_load,
    Context, RESTARTED_AT,
};
use chrono::{DateTime, NaiveDateTime, Offset};
use k8s_openapi::{api::core::v1::Pod, apimachinery::pkg::apis::meta::v1::ObjectMeta};
use kube::{
    api::{DeleteParams, ListParams, Patch, PatchParams},
    runtime::{controller::Action, wait::Condition},
    Api, Resource, ResourceExt,
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use tokio::time::Duration;
use tracing::{debug, error, info, instrument, warn};

pub struct PostgresConfig {
    pub postgres_parameters: Option<BTreeMap<String, String>>,
    pub shared_preload_libraries: Option<Vec<String>>,
}

fn create_cluster_backup_barman_data(cdb: &CoreDB) -> Option<ClusterBackupBarmanObjectStoreData> {
    let encryption = match &cdb.spec.backup.encryption {
        Some(encryption) => match encryption.as_str() {
            "AES256" => Some(ClusterBackupBarmanObjectStoreDataEncryption::Aes256),
            "aws:kms" => Some(ClusterBackupBarmanObjectStoreDataEncryption::AwsKms),
            _ => None,
        },
        _ => None,
    };

    Some(ClusterBackupBarmanObjectStoreData {
        compression: Some(ClusterBackupBarmanObjectStoreDataCompression::Snappy),
        encryption,
        immediate_checkpoint: Some(true),
        ..ClusterBackupBarmanObjectStoreData::default()
    })
}

fn create_cluster_backup_barman_wal(cdb: &CoreDB) -> Option<ClusterBackupBarmanObjectStoreWal> {
    let encryption = match &cdb.spec.backup.encryption {
        Some(encryption) => match encryption.as_str() {
            "AES256" => Some(ClusterBackupBarmanObjectStoreWalEncryption::Aes256),
            "aws:kms" => Some(ClusterBackupBarmanObjectStoreWalEncryption::AwsKms),
            _ => None,
        },
        _ => None,
    };

    if encryption.is_some() {
        Some(ClusterBackupBarmanObjectStoreWal {
            compression: Some(ClusterBackupBarmanObjectStoreWalCompression::Snappy),
            encryption,
            max_parallel: Some(5),
        })
    } else {
        None
    }
}

fn create_cluster_backup_barman_object_store(
    cdb: &CoreDB,
    endpoint_url: &str,
    backup_path: &str,
    s3_credentials: &ClusterBackupBarmanObjectStoreS3Credentials,
) -> ClusterBackupBarmanObjectStore {
    ClusterBackupBarmanObjectStore {
        data: create_cluster_backup_barman_data(cdb),
        endpoint_url: Some(endpoint_url.to_string()),
        destination_path: backup_path.to_string(),
        s3_credentials: Some(s3_credentials.clone()),
        wal: create_cluster_backup_barman_wal(cdb),
        ..ClusterBackupBarmanObjectStore::default()
    }
}

fn create_cluster_certificates(cdb: &CoreDB) -> Option<ClusterCertificates> {
    let name = cdb.metadata.name.clone().unwrap();
    match std::env::var("USE_SHARED_CA") {
        Ok(_) => {
            debug!(
                "USE_SHARED_CA is set, including certificate in CNPG spec: {}",
                name
            );
            Some(ClusterCertificates {
                client_ca_secret: Some(format!("{}-ca1", name)),
                server_ca_secret: Some(format!("{}-ca1", name)),
                replication_tls_secret: Some(format!("{}-replication1", name)),
                server_tls_secret: Some(format!("{}-server1", name)),
                ..ClusterCertificates::default()
            })
        }
        Err(_) => {
            debug!(
                "USE_SHARED_CA not set, not including certificate in CNPG spec: {}",
                name
            );
            None
        }
    }
}

fn create_cluster_backup(
    cdb: &CoreDB,
    endpoint_url: &str,
    backup_path: &str,
    s3_credentials: &ClusterBackupBarmanObjectStoreS3Credentials,
) -> Option<ClusterBackup> {
    let retention_days = match &cdb.spec.backup.retentionPolicy {
        None => "30d".to_string(),
        Some(retention_policy) => match retention_policy.parse::<i32>() {
            Ok(days) => {
                format!("{}d", days)
            }
            Err(_) => {
                warn!("Invalid retention policy because could not convert to i32, using default of 30 days");
                "30d".to_string()
            }
        },
    };

    Some(ClusterBackup {
        barman_object_store: Some(create_cluster_backup_barman_object_store(
            cdb,
            endpoint_url,
            backup_path,
            s3_credentials,
        )),
        retention_policy: Some(retention_days), // Adjust as needed
        ..ClusterBackup::default()
    })
}

pub fn cnpg_backup_configuration(
    cdb: &CoreDB,
    cfg: &Config,
) -> (Option<ClusterBackup>, Option<ClusterServiceAccountTemplate>) {
    let mut service_account_template: Option<ClusterServiceAccountTemplate> = None;

    // Check if backups are enabled
    if !cfg.enable_backup {
        return (None, None);
    }

    debug!("Backups are enabled, configuring...");

    // Check for backup path
    let backup_path = cdb.spec.backup.destinationPath.clone();
    if backup_path.is_none() {
        warn!("Backups are disabled because we don't have an S3 backup path");
        return (None, None);
    }

    let should_set_service_account_template = (cdb.spec.backup.endpoint_url.is_none()
        && cdb.spec.backup.s3_credentials.is_none())
        || (cdb
            .spec
            .backup
            .s3_credentials
            .as_ref()
            .and_then(|cred| cred.inherit_from_iam_role)
            .unwrap_or(false)
            && cdb
                .spec
                .serviceAccountTemplate
                .metadata
                .as_ref()
                .and_then(|meta| meta.annotations.as_ref())
                .map_or(false, |annots| {
                    annots.contains_key("eks.amazonaws.com/role-arn")
                }));

    let should_reset_service_account_template = cdb
        .spec
        .backup
        .s3_credentials
        .as_ref()
        .and_then(|cred| cred.inherit_from_iam_role)
        == Some(false)
        && (cdb
            .spec
            .backup
            .s3_credentials
            .as_ref()
            .map_or(false, |cred| {
                cred.access_key_id.is_some()
                    || cred.region.is_some()
                    || cred.secret_access_key.is_some()
                    || cred.session_token.is_some()
            }));

    if should_reset_service_account_template {
        service_account_template = None;
    } else if should_set_service_account_template {
        let service_account_metadata = cdb.spec.serviceAccountTemplate.metadata.clone();
        if service_account_metadata.is_none() {
            warn!("Backups are disabled because we don't have a service account template");
            return (None, None);
        }
        let service_account_annotations = service_account_metadata
            .expect("Expected service account template metadata")
            .annotations;
        if service_account_annotations.is_none() {
            warn!("Backups are disabled because we don't have a service account template with annotations");
            return (None, None);
        }
        let annotations =
            service_account_annotations.expect("Expected service account template annotations");
        let service_account_role_arn = annotations.get("eks.amazonaws.com/role-arn");
        if service_account_role_arn.is_none() {
            warn!(
                "Backups are disabled because we don't have a service account template with an EKS role ARN"
            );
            return (None, None);
        }
        let role_arn = service_account_role_arn
            .expect("Expected service account template annotations to contain an EKS role ARN")
            .clone();

        service_account_template = Some(ClusterServiceAccountTemplate {
            metadata: ClusterServiceAccountTemplateMetadata {
                annotations: Some(BTreeMap::from([(
                    "eks.amazonaws.com/role-arn".to_string(),
                    role_arn,
                )])),
                ..ClusterServiceAccountTemplateMetadata::default()
            },
        });
    }
    // Copy the endpoint_url and s3_credentials from cdb to configure backups
    let endpoint_url = cdb.spec.backup.endpoint_url.as_deref().unwrap_or_default();
    let s3_credentials = generate_s3_backup_credentials(cdb.spec.backup.s3_credentials.as_ref());
    let cluster_backup =
        create_cluster_backup(cdb, endpoint_url, &backup_path.unwrap(), &s3_credentials);

    (cluster_backup, service_account_template)
}

// parse_target_time returns the parsed target_time which is used for point-in-time-recovery
// Currently, we support formats of target_time as follows (Basically support what CNPG supports):
// YYYY-MM-DD HH24:MI:SS
// YYYY-MM-DD HH24:MI:SS.FF6TZH
// YYYY-MM-DD HH24:MI:SS.FF6TZH:TZM
// YYYY-MM-DDTHH24:MI:SSZ            (RFC3339)
// YYYY-MM-DDTHH24:MI:SS±TZH:TZM     (RFC3339)
// YYYY-MM-DDTHH24:MI:SSS±TZH:TZM	   (RFC3339Micro)
// YYYY-MM-DDTHH24:MI:SS             (modified RFC3339)
fn parse_target_time(target_time: Option<&str>) -> Result<Option<String>, ValueError> {
    if let Some(time_str) = target_time {
        // Try to parse the target_time with the following formats in order
        // 1. YYYY-MM-DD HH24:MI:SS
        let result = NaiveDateTime::parse_from_str(time_str, "%Y-%m-%d %H:%M:%S")
            .map(|dt| dt.to_string())
            .or_else(|_| {
                // 2. YYYY-MM-DD HH24:MI:SS.FF6TZH
                // 3. YYYY-MM-DD HH24:MI:SS.FF6TZH:TZM
                DateTime::parse_from_str(time_str, "%Y-%m-%d %H:%M:%S%.6f%z").map(|dt| {
                    let offset_hours = dt.offset().fix().local_minus_utc() / 3600;
                    let formatted_offset = format!("{:+03}", offset_hours);
                    format!(
                        "{}.{:06}{}",
                        dt.format("%Y-%m-%d %H:%M:%S"),
                        dt.timestamp_subsec_micros(),
                        formatted_offset
                    )
                })
            })
            .or_else(|_| {
                // 4. YYYY-MM-DDTHH24:MI:SSZ            (RFC3339)
                // 5. YYYY-MM-DDTHH24:MI:SS±TZH:TZM     (RFC3339)
                // 6. YYYY-MM-DDTHH24:MI:SSS±TZH:TZM    (RFC3339Micro)
                // 7. YYYY-MM-DDTHH24:MI:SS             (modified RFC3339)
                DateTime::parse_from_rfc3339(time_str).map(|dt| {
                    let offset_hours = dt.offset().fix().local_minus_utc() / 3600;
                    let formatted_offset = format!("{:+03}", offset_hours);
                    format!(
                        "{}.{:06}{}",
                        dt.format("%Y-%m-%d %H:%M:%S"),
                        dt.timestamp_subsec_micros(),
                        formatted_offset
                    )
                })
            });

        // Return the parsed target_time if it is Ok, otherwise return ValueError
        // todo: Somehow turn this into a requeue action, so that we can retry
        //      when the target_time is not in the correct format.
        match result {
            Ok(parsed_time) => Ok(Some(parsed_time)),
            Err(err) => Err(ValueError::ChronoParseError(err)),
        }
    } else {
        Ok(None)
    }
}

#[instrument(skip(cdb))]
pub fn cnpg_cluster_bootstrap_from_cdb(
    cdb: &CoreDB,
) -> (
    Option<ClusterBootstrap>,
    Option<Vec<ClusterExternalClusters>>,
    Option<ClusterSuperuserSecret>,
) {
    // parse_target_time returns the parsed target_time which is used for point-in-time-recovery
    // todo: Somehow turn this into a requeue action, so that we can retry when the target_time is not in the correct format.
    //      for now we just log the error and return None, which will disable point-in-time-recovery, but allow for a full recovery
    let parsed_target_time = cdb.spec.restore.as_ref().and_then(|restore| {
        restore.recovery_target_time.as_ref().and_then(|time_str| {
            match parse_target_time(Some(time_str)) {
                Ok(Some(parsed_time)) => Some(parsed_time),
                Ok(None) => None,
                Err(err) => {
                    error!(
                        "Failed to parse target_time for instance: {}, {}",
                        cdb.name_any(),
                        err
                    );
                    None
                }
            }
        })
    });

    let cluster_bootstrap = if let Some(_restore) = &cdb.spec.restore {
        ClusterBootstrap {
            recovery: Some(ClusterBootstrapRecovery {
                source: Some("tembo-recovery".to_string()),
                database: Some("app".to_string()),
                owner: Some("app".to_string()),
                recovery_target: parsed_target_time.map(|target_time| {
                    ClusterBootstrapRecoveryRecoveryTarget {
                        target_time: Some(target_time),
                        ..ClusterBootstrapRecoveryRecoveryTarget::default()
                    }
                }),
                ..ClusterBootstrapRecovery::default()
            }),
            ..ClusterBootstrap::default()
        }
    } else {
        ClusterBootstrap {
            initdb: Some(ClusterBootstrapInitdb {
                ..ClusterBootstrapInitdb::default()
            }),
            ..ClusterBootstrap::default()
        }
    };
    let cluster_name = cdb.name_any();

    let mut coredb_connection_parameters = BTreeMap::new();
    coredb_connection_parameters.insert("user".to_string(), "postgres".to_string());
    // The CoreDB operator rw service name is the CoreDB cluster name
    coredb_connection_parameters.insert("host".to_string(), cluster_name.clone());

    let superuser_secret_name = format!("{}-connection", cluster_name);

    let coredb_cluster = if let Some(restore) = &cdb.spec.restore {
        let s3_credentials = generate_s3_restore_credentials(restore.s3_credentials.as_ref());
        // Find destination_path from Backup to generate the restore destination path
        let restore_destination_path = match &cdb.spec.backup.destinationPath {
            Some(path) => generate_restore_destination_path(path),
            None => "".to_string(), // or any other default value you'd like
        };
        ClusterExternalClusters {
            name: "tembo-recovery".to_string(),
            barman_object_store: Some(ClusterExternalClustersBarmanObjectStore {
                destination_path: format!("{}/{}", restore_destination_path, restore.server_name),
                endpoint_url: restore.endpoint_url.clone(),
                s3_credentials: Some(s3_credentials),
                wal: Some(ClusterExternalClustersBarmanObjectStoreWal {
                    max_parallel: Some(5),
                    encryption: Some(ClusterExternalClustersBarmanObjectStoreWalEncryption::Aes256),
                    compression: Some(
                        ClusterExternalClustersBarmanObjectStoreWalCompression::Snappy,
                    ),
                }),
                server_name: Some(restore.server_name.clone()),
                ..ClusterExternalClustersBarmanObjectStore::default()
            }),
            ..ClusterExternalClusters::default()
        }
    } else {
        ClusterExternalClusters {
            name: "coredb".to_string(),
            connection_parameters: Some(coredb_connection_parameters),
            password: Some(ClusterExternalClustersPassword {
                // The CoreDB operator connection secret is named as the cluster
                // name, suffixed by -connection
                name: Some(superuser_secret_name.clone()),
                key: "password".to_string(),
                ..ClusterExternalClustersPassword::default()
            }),
            ..ClusterExternalClusters::default()
        }
    };

    let superuser_secret = ClusterSuperuserSecret {
        name: superuser_secret_name,
    };

    (
        Some(cluster_bootstrap),
        Some(vec![coredb_cluster]),
        Some(superuser_secret),
    )
}

// Get PGConfig from CoreDB and convert it to a postgres_parameters and shared_preload_libraries
fn cnpg_postgres_config(
    cdb: &CoreDB,
    requires_load: BTreeMap<String, String>,
) -> Result<PostgresConfig, MergeError> {
    match cdb.spec.get_pg_configs(requires_load) {
        Ok(Some(pg_configs)) => {
            let mut postgres_parameters: BTreeMap<String, String> = BTreeMap::new();
            let mut shared_preload_libraries: Vec<String> = Vec::new();

            for pg_config in pg_configs {
                match &pg_config.name[..] {
                    "shared_preload_libraries" => {
                        shared_preload_libraries.push(pg_config.value.to_string());
                    }
                    _ => {
                        postgres_parameters
                            .insert(pg_config.name.clone(), pg_config.value.to_string());
                    }
                }
            }

            let params = if postgres_parameters.is_empty() {
                None
            } else {
                Some(postgres_parameters)
            };

            let libs = if shared_preload_libraries.is_empty() {
                None
            } else {
                Some(shared_preload_libraries)
            };

            Ok(PostgresConfig {
                postgres_parameters: params,
                shared_preload_libraries: libs,
            })
        }
        Ok(None) => {
            // Return None, None when no pg_config is set
            Ok(PostgresConfig {
                postgres_parameters: None,
                shared_preload_libraries: None,
            })
        }
        Err(e) => Err(e),
    }
}

fn cnpg_cluster_storage(cdb: &CoreDB) -> Option<ClusterStorage> {
    let storage = cdb.spec.storage.clone().0;
    let storage_class = cnpg_cluster_storage_class(cdb);
    Some(ClusterStorage {
        resize_in_use_volumes: Some(true),
        size: Some(storage),
        storage_class,
        ..ClusterStorage::default()
    })
}

fn cnpg_cluster_storage_class(cdb: &CoreDB) -> Option<String> {
    match &cdb.spec.storage_class {
        Some(storage_class) if !storage_class.is_empty() => Some(storage_class.clone()),
        _ => None,
    }
}

// Check replica count to enable HA
fn cnpg_high_availability(cdb: &CoreDB) -> Option<ClusterReplicationSlots> {
    if cdb.spec.replicas > 1 {
        Some(ClusterReplicationSlots {
            high_availability: Some(ClusterReplicationSlotsHighAvailability {
                enabled: Some(true),
                ..ClusterReplicationSlotsHighAvailability::default()
            }),
            update_interval: Some(30),
        })
    } else {
        Some(ClusterReplicationSlots {
            high_availability: Some(ClusterReplicationSlotsHighAvailability {
                enabled: Some(false),
                ..ClusterReplicationSlotsHighAvailability::default()
            }),
            update_interval: Some(30),
        })
    }
}

fn default_cluster_annotations(cdb: &CoreDB) -> BTreeMap<String, String> {
    let mut annotations = cdb.metadata.annotations.clone().unwrap_or_default();
    annotations.insert(
        "tembo-pod-init.tembo.io/inject".to_string(),
        "true".to_string(),
    );
    // If the annotation tembo.io/org_id is present, rename it to tembo.io/organization_id
    if let Some(org_id) = annotations.remove("tembo.io/org_id") {
        annotations.insert("tembo.io/organization_id".to_string(), org_id);
    }
    annotations
}

pub fn cnpg_cluster_from_cdb(
    cdb: &CoreDB,
    fenced_pods: Option<Vec<String>>,
    requires_load: BTreeMap<String, String>,
) -> Cluster {
    let cfg = Config::default();
    let name = cdb.name_any();
    let namespace = cdb.namespace().unwrap();
    let owner_reference = cdb.controller_owner_ref(&()).unwrap();
    let mut annotations = default_cluster_annotations(cdb);
    let (bootstrap, external_clusters, superuser_secret) = cnpg_cluster_bootstrap_from_cdb(cdb);
    let (backup, service_account_template) = cnpg_backup_configuration(cdb, &cfg);
    let storage = cnpg_cluster_storage(cdb);
    let replication = cnpg_high_availability(cdb);

    let PostgresConfig {
        postgres_parameters,
        shared_preload_libraries,
    } = match cnpg_postgres_config(cdb, requires_load) {
        Ok(config) => config,
        Err(e) => {
            error!("Error generating postgres parameters: {}", e);
            PostgresConfig {
                postgres_parameters: None,
                shared_preload_libraries: None,
            }
        }
    };

    // Format fenced pods annotation if we have any
    if let Some(fenced_pods) = fenced_pods {
        let fenced_instances = format!("{:?}", fenced_pods);
        annotations.insert("cnpg.io/fencedInstances".to_string(), fenced_instances);
    }
    debug!("Annotations: {:?}", annotations);

    // set the container image
    // Check if the cdb.spec.image is set, if not then figure out which image to use.
    let image = if cdb.spec.image.is_empty() {
        match cdb.spec.stack.as_ref().map(|s| s.name.to_lowercase()) {
            Some(ref name) if name == "machinelearning" => default_llm_image_uri(),
            Some(ref name) if name == "datawarehouse" => default_dw_image_uri(),
            _ => default_image_uri(),
        }
    } else {
        cdb.spec.image.clone()
    };

    let certificates = create_cluster_certificates(cdb);
    let mut metrics = vec![ClusterMonitoringCustomQueriesConfigMap {
        key: "queries".to_string(),
        name: "cnpg-default-monitoring".to_string(),
    }];

    if custom_metrics_configmap_settings().is_some() {
        let configmap_name = format!("{}-custom", cdb.name_any());
        metrics.push(ClusterMonitoringCustomQueriesConfigMap {
            key: "custom-queries".to_string(),
            name: configmap_name,
        })
    }

    if cdb
        .spec
        .metrics
        .as_ref()
        .and_then(|m| m.queries.as_ref())
        .is_some()
    {
        let configmap = format!("{}{}", EXPORTER_CONFIGMAP_PREFIX, cdb.name_any());
        metrics.push(ClusterMonitoringCustomQueriesConfigMap {
            key: "tembo-queries".to_string(),
            name: configmap,
        })
    }

    Cluster {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(namespace),
            annotations: Some(annotations),
            owner_references: Some(vec![owner_reference]),
            ..ObjectMeta::default()
        },
        spec: ClusterSpec {
            affinity: Some(ClusterAffinity {
                pod_anti_affinity_type: Some("preferred".to_string()),
                topology_key: Some("topology.kubernetes.io/zone".to_string()),
                ..ClusterAffinity::default()
            }),
            backup,
            service_account_template,
            bootstrap,
            certificates,
            superuser_secret,
            external_clusters,
            enable_superuser_access: Some(true),
            failover_delay: Some(0),
            image_name: Some(image),
            instances: cdb.spec.replicas as i64,
            log_level: Some(ClusterLogLevel::Info),
            managed: cluster_managed(&name),
            max_sync_replicas: Some(0),
            min_sync_replicas: Some(0),
            monitoring: Some(ClusterMonitoring {
                custom_queries_config_map: Some(metrics),
                disable_default_queries: Some(false),
                enable_pod_monitor: Some(true),
                ..ClusterMonitoring::default()
            }),
            postgres_gid: Some(26),
            postgres_uid: Some(26),
            postgresql: Some(ClusterPostgresql {
                ldap: None,
                parameters: postgres_parameters,
                sync_replica_election_constraint: Some(
                    ClusterPostgresqlSyncReplicaElectionConstraint {
                        enabled: false,
                        ..ClusterPostgresqlSyncReplicaElectionConstraint::default()
                    },
                ),
                shared_preload_libraries,
                pg_hba: None,
                enable_alter_system: Some(true),
                ..ClusterPostgresql::default()
            }),
            primary_update_method: Some(ClusterPrimaryUpdateMethod::Restart),
            primary_update_strategy: Some(ClusterPrimaryUpdateStrategy::Unsupervised),
            replication_slots: replication,
            resources: Some(ClusterResources {
                claims: None,
                limits: cdb.spec.resources.clone().limits,
                requests: cdb.spec.resources.clone().requests,
            }),
            // The time in seconds that is allowed for a PostgreSQL instance to successfully start up
            start_delay: Some(30),
            // The time in seconds that is allowed for a PostgreSQL instance to gracefully shutdown
            stop_delay: Some(30),
            // The maximum time window in seconds within the stopDelay value reserved to complete the smart shutdown procedure in PostgreSQL
            smart_shutdown_timeout: Some(15),
            storage,
            // The time in seconds that is allowed for a primary PostgreSQL instance
            // to gracefully shutdown during a switchover
            switchover_delay: Some(60),
            // Set this to match when the cluster consolidation happens
            node_maintenance_window: Some(ClusterNodeMaintenanceWindow {
                // TODO TEM-1407: Make this configurable and aligned with cluster scale down
                // default to in_progress: true - otherwise single-instance CNPG clusters
                // prevent cluster scale down.
                in_progress: Some(true),
                ..ClusterNodeMaintenanceWindow::default()
            }),
            ..ClusterSpec::default()
        },
        status: None,
    }
}

fn cluster_managed(name: &str) -> Option<ClusterManaged> {
    Some(ClusterManaged {
        roles: Some(vec![
            ClusterManagedRoles {
                name: "readonly".to_string(),
                ensure: Some(ClusterManagedRolesEnsure::Present),
                login: Some(true),
                password_secret: Some(ClusterManagedRolesPasswordSecret {
                    name: format!("{}-ro", name).to_string(),
                }),
                in_roles: Some(vec!["pg_read_all_data".to_string()]),
                ..ClusterManagedRoles::default()
            },
            ClusterManagedRoles {
                name: "postgres_exporter".to_string(),
                ensure: Some(ClusterManagedRolesEnsure::Present),
                login: Some(true),
                password_secret: Some(ClusterManagedRolesPasswordSecret {
                    name: format!("{}-exporter", name).to_string(),
                }),
                in_roles: Some(vec![
                    "pg_read_all_stats".to_string(),
                    "pg_monitor".to_string(),
                ]),
                ..ClusterManagedRoles::default()
            },
        ]),
    })
}

// This is a synchronous function that takes the latest_generated_node and diff_instances
// and returns a Vec<String> containing the names of the pods to be fenced.
#[instrument(fields(trace_id))]
fn calculate_pods_to_fence(
    latest_generated_node: i32,
    diff_instances: i32,
    base_name: &str,
) -> Vec<String> {
    let mut pod_names_to_fence = Vec::new();
    for i in 1..=diff_instances {
        let pod_to_fence = latest_generated_node + i;
        let pod_name = format!("{}-{}", base_name, pod_to_fence);
        pod_names_to_fence.push(pod_name);
    }
    pod_names_to_fence
}

// This is a synchronous function to extend pod_names_to_fence with fenced_pods.
#[instrument(fields(trace_id))]
fn extend_with_fenced_pods(pod_names_to_fence: &mut Vec<String>, fenced_pods: Option<Vec<String>>) {
    if let Some(fenced_pods) = fenced_pods {
        pod_names_to_fence.extend(fenced_pods);
    }
}

// pods_to_fence determines a list of pod names that should be fenced when we detect that new replicas are being created
#[instrument(skip(cdb, ctx), fields(trace_id, instance_name = %cdb.name_any()))]
async fn pods_to_fence(cdb: &CoreDB, ctx: Arc<Context>) -> Result<Vec<String>, Action> {
    // Check if there is an initial backup running, pending or completed.  We
    // should never fence a pod with an active initial backup running, pending or
    // completed.  There could be a time where we go into a reconcile loop
    // during a restore, where the first_recoverability_time is not set and a backup
    // is running.  We need to exit early in that case and not fence the running pod.
    if cdb.spec.restore.is_some()
        && is_restore_backup_running_pending_completed(cdb, ctx.clone())
            .await
            .unwrap_or(false)
    {
        debug!(
            "Running or pending backup detected for instance {}, skipping fencing of pods.",
            &cdb.name_any()
        );
        return Ok(Vec::new());
    }

    // Check if a restore is requested
    if cdb.spec.restore.is_some()
        && cdb
            .status
            .as_ref()
            .and_then(|s| s.first_recoverability_time.as_ref())
            .is_none()
        && cdb
            .status
            .as_ref()
            .and_then(|s| s.last_fully_reconciled_at.as_ref())
            .is_none()
    {
        // If restore is requested, fence all the pods based on the cdb.spec.replicas value
        let mut pod_names_to_fence = Vec::new();
        for i in 1..=cdb.spec.replicas {
            let pod_name = format!("{}-{}", &cdb.name_any(), i);
            pod_names_to_fence.push(pod_name);
        }

        debug!(
            "Restore requested. Pods to be fenced during restore: {:?}",
            pod_names_to_fence
        );
        return Ok(pod_names_to_fence);
    }

    // Get replica count from CoreDBSpec
    let cdb_replica = cdb.spec.replicas;

    // using get_cluster_replicas function to lookup current replica count from Cluster object
    let cluster_replica_result = get_instance_replicas(cdb, ctx.clone()).await;

    let mut pod_names_to_fence = Vec::new();

    // Since cluster_replica_result is a i64, we need to convert to i32 to compare with cdb_replica
    match cluster_replica_result {
        Ok(replica) => {
            let cluster_replica: i32 = replica.try_into().unwrap();

            if cdb_replica > cluster_replica {
                match get_latest_generated_node(cdb, ctx.clone(), &cdb.name_any()).await {
                    Ok(Some(latest_generated_node)) => {
                        debug!("Latest generated node: {:?}", latest_generated_node.clone());

                        match latest_generated_node.parse::<i32>() {
                            Ok(latest_generated_node) => {
                                let diff_instances = cdb_replica - cluster_replica;
                                let pod_names_to_fence = calculate_pods_to_fence(
                                    latest_generated_node,
                                    diff_instances,
                                    &cdb.name_any(),
                                );

                                debug!("Pods to be fenced: {:?}", pod_names_to_fence);
                                Ok(pod_names_to_fence)
                            }
                            Err(_) => {
                                error!("Failed to parse latest_generated_node as an integer");
                                Err(Action::requeue(Duration::from_secs(300)))
                            }
                        }
                    }
                    Ok(None) => {
                        warn!("Latest generated node is not available yet for instance {}. It might be a new or initializing cluster.", &cdb.name_any());
                        Err(Action::requeue(Duration::from_secs(30)))
                    }
                    Err(e) => {
                        error!("Error getting latest generated node: {:?}", e);
                        Err(Action::requeue(Duration::from_secs(300)))
                    }
                }
            } else {
                debug!("Replica count is the same, lookup annotation for fenced pods");

                let fenced_pods = get_fenced_pods(cdb, ctx.clone()).await?;
                extend_with_fenced_pods(&mut pod_names_to_fence, fenced_pods);

                Ok(pod_names_to_fence)
            }
        }
        Err(_) => {
            if cdb_replica > 1 {
                // Logic for fencing when cluster_replica is non-existent but cdb_replica > 1
                // reuse or adapt the existing logic here for when cluster_replica exists
                // ...

                // get_latest_generated_node will not be present or set in the cluster
                // at this point.  So we will need to do this another way if the replicas > 1
                let mut pod_names_to_fence = Vec::new();

                for i in 2..=cdb_replica {
                    // Start from 2 as per your example
                    let pod_name = format!("{}-{}", &cdb.name_any(), i);
                    pod_names_to_fence.push(pod_name);
                }

                // Debug log to check the names of the pods to fence
                debug!("Pods to be fenced: {:?}", pod_names_to_fence);
                Ok(pod_names_to_fence)
            } else {
                // Cluster is bootstrapping and cdb_replica <= 1, so no fencing is needed
                debug!("Cluster is bootstrapping and cdb_replica <= 1, skipping fencing.");
                Ok(pod_names_to_fence)
            }
        }
    }
}

// cdb: the CoreDB object
// maybe_cluster, Option<Cluster> of the current CNPG cluster, if it exists
// new_spec: the new Cluster spec to be applied
fn update_restarted_at(
    cdb: &CoreDB,
    maybe_cluster: Option<&Cluster>,
    new_spec: &mut Cluster,
) -> bool {
    let Some(cdb_restarted_at) = cdb.annotations().get(RESTARTED_AT) else {
        // No need to update the annotation if it's not present in the CoreDB
        return false;
    };

    // Remember the previous value of the annotation, if any
    let previous_restarted_at =
        maybe_cluster.and_then(|cluster| cluster.annotations().get(RESTARTED_AT));

    // Forward the `restartedAt` annotation from CoreDB over to the CNPG cluster,
    // does not matter if changed or not.
    new_spec
        .metadata
        .annotations
        .as_mut()
        .map(|cluster_annotations| {
            cluster_annotations.insert(RESTARTED_AT.into(), cdb_restarted_at.to_owned())
        });

    let restart_annotation_updated = previous_restarted_at != Some(cdb_restarted_at);

    if restart_annotation_updated {
        let name = new_spec.metadata.name.as_deref().unwrap_or("unknown");
        info!("restartAt changed for cluster {name}, setting to {cdb_restarted_at}.");
    }

    restart_annotation_updated
}

#[instrument(skip(cdb, ctx) fields(trace_id, instance_name = %cdb.name_any()))]
pub async fn reconcile_cnpg(cdb: &CoreDB, ctx: Arc<Context>) -> Result<(), Action> {
    let pods_to_fence = pods_to_fence(cdb, ctx.clone()).await?;
    let requires_load =
        extensions_that_require_load(ctx.client.clone(), &cdb.metadata.namespace.clone().unwrap())
            .await?;

    debug!("Generating CNPG spec");
    let mut cluster = cnpg_cluster_from_cdb(cdb, Some(pods_to_fence), requires_load);

    debug!("Getting namespace of cluster");
    let namespace = cluster
        .metadata
        .namespace
        .clone()
        .expect("CNPG Cluster should always have a namespace");
    debug!("Getting name of cluster");
    let name = cluster
        .metadata
        .name
        .clone()
        .expect("CNPG Cluster should always have a name");

    let cluster_api: Api<Cluster> = Api::namespaced(ctx.client.clone(), namespace.as_str());
    let maybe_cluster = cluster_api.get(&name).await;

    let restart_annotation_updated =
        update_restarted_at(cdb, maybe_cluster.as_ref().ok(), &mut cluster);

    let mut _restart_required = false;

    match cluster
        .spec
        .postgresql
        .clone()
        .expect("We always set the postgresql spec")
        .shared_preload_libraries
    {
        None => {
            debug!("We are not setting any shared_preload_libraries");
        }
        Some(new_libs) => {
            debug!("We are setting shared_preload_libraries, so we have to check if the files are already installed");
            match maybe_cluster {
                Ok(current_cluster) => {
                    let current_shared_preload_libraries = match current_cluster
                        .spec
                        .postgresql
                        .clone()
                        .expect("We always set postgresql cluster spec")
                        .shared_preload_libraries
                    {
                        None => {
                            let current_libs: Vec<String> = vec![];
                            current_libs
                        }
                        Some(current_libs) => current_libs,
                    };
                    // Check if current_shared_preload_libraries and new_libs are the same
                    if current_shared_preload_libraries != new_libs {
                        let mut libs_that_are_installed: Vec<String> = vec![];
                        // If we can't find the existing primary pod, returns a requeue
                        let primary_pod_cnpg = cdb
                            .primary_pod_cnpg_ready_or_not(ctx.client.clone())
                            .await?;
                        // Check if the file is already installed
                        let command = vec![
                            "/bin/sh".to_string(),
                            "-c".to_string(),
                            "ls $(pg_config --pkglibdir)".to_string(),
                        ];
                        let result = cdb
                            .exec(primary_pod_cnpg.name_any(), ctx.client.clone(), &command)
                            .await
                            .map_err(|e| {
                                error!("Error checking for presence of extension files: {:?}", e);
                                Action::requeue(Duration::from_secs(30))
                            })?;
                        let available_libs = match result.stdout {
                            None => {
                                error!("Error checking for presence of extension files");
                                return Err(Action::requeue(Duration::from_secs(30)));
                            }
                            Some(output) => output
                                .split('\n')
                                .map(|s| s.to_string())
                                .collect::<Vec<String>>(),
                        };
                        for libs in new_libs {
                            let split_libs = libs
                                .split(',')
                                .map(|s| s.to_string())
                                .collect::<Vec<String>>();
                            for new_lib in split_libs {
                                if available_libs.contains(&format!("{}.so", new_lib)) {
                                    info!("Changing shared_preload_libraries on {}, found {} is installed, so including it", &name, &new_lib);
                                    libs_that_are_installed.push(new_lib.clone());
                                    if !current_shared_preload_libraries.contains(&new_lib) {
                                        _restart_required = true;
                                    }
                                } else {
                                    info!("Changing shared_preload_libraries on {}, found {} is NOT installed, so dropping it", &name, &new_lib);
                                }
                            }
                        }
                        if let Some(postgresql) = cluster.spec.postgresql.as_mut() {
                            postgresql.shared_preload_libraries = Some(libs_that_are_installed);
                        }
                    }
                }
                Err(_) => {
                    // Here, we should drop all shared_preload_libraries
                    if let Some(postgresql) = cluster.spec.postgresql.as_mut() {
                        info!(
                            "We are dropping all shared_preload_libraries for initial creation of Cluster {}",
                            &name
                        );
                        postgresql.shared_preload_libraries = None;
                    }
                }
            }
        }
    }

    // For manual changes conflicting with the operator, we have .force()
    //
    // When trying to apply an object, fields that have a different value and are owned by another manager will result in a conflict.
    // This is done in order to signal that the operation might undo another collaborator's changes.
    // Writes to objects with managed fields can be forced, in which case the value of any conflicted field will be overridden,
    // and the ownership will be transferred.
    // https://kubernetes.io/docs/reference/using-api/server-side-apply/
    let ps = PatchParams::apply("cntrlr").force();

    let _o = cluster_api
        .patch(&name, &ps, &Patch::Apply(&cluster))
        .await
        .map_err(|e| {
            error!("Error patching cluster: {}", e);
            Action::requeue(Duration::from_secs(300))
        })?;

    // If we updated the restartedAt annotation, set `status.running` in CoreDB to false
    if restart_annotation_updated {
        let cdb_cluster: Api<CoreDB> = Api::namespaced(ctx.client.clone(), &namespace);
        let cluster_name = &name;

        patch_cdb_status_merge(
            &cdb_cluster,
            cluster_name,
            json!({
                "status": {
                    "running": false
                }
            }),
        )
        .await?;
        info!(
            "Updated status.running to false in {}, requeuing 10 seconds",
            &name
        );
        return Err(Action::requeue(Duration::from_secs(10)));
    }

    Ok(())
}

// Reconcile a Pooler
#[instrument(skip(cdb, ctx) fields(trace_id, instance_name = %cdb.name_any()))]
pub async fn reconcile_pooler(cdb: &CoreDB, ctx: Arc<Context>) -> Result<(), Action> {
    let client = ctx.client.clone();
    let name = cdb.name_any() + "-pooler";
    let namespace = cdb.namespace().unwrap();
    let pooler_api: Api<Pooler> = Api::namespaced(client.clone(), namespace.as_str());

    let owner_reference = cdb.controller_owner_ref(&()).unwrap();

    // If pooler is enabled, create or update
    if cdb.spec.connectionPooler.enabled {
        debug!("Configuraing pooler instance for {}", cdb.name_any());
        let pooler = Pooler {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(namespace.clone()),
                owner_references: Some(vec![owner_reference]),
                ..ObjectMeta::default()
            },
            spec: PoolerSpec {
                cluster: PoolerCluster {
                    name: cdb.name_any(),
                },
                deployment_strategy: None,
                instances: Some(1),
                monitoring: None,
                pgbouncer: PoolerPgbouncer {
                    auth_query: None,
                    auth_query_secret: None,
                    parameters: cdb.spec.connectionPooler.pooler.parameters.clone(),
                    paused: None,
                    pg_hba: None,
                    pool_mode: Some(cdb.spec.connectionPooler.pooler.poolMode.clone()),
                },
                template: Some(PoolerTemplate {
                    metadata: None,
                    spec: Some(PoolerTemplateSpec {
                        containers: vec![PoolerTemplateSpecContainers {
                            name: "pgbouncer".to_string(),
                            resources: cdb.spec.connectionPooler.pooler.resources.clone(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                }),
                r#type: Some(PoolerType::Rw),
            },
            status: None,
        };

        debug!("Patching Pooler {name}");
        let ps = PatchParams::apply("cntrlr").force();
        let _o = pooler_api
            .patch(&name, &ps, &Patch::Apply(&pooler))
            .await
            .map_err(|e| {
                error!("Error patching Pooler: {}", e);
                Action::requeue(Duration::from_secs(300))
            })?;

        // Check to see if the primary pod is ready, if it is the setup pgbouncer.  If the pod is
        // not ready then just continue on and wait for the next reconcile.
        let primary_pod = cdb.primary_pod_cnpg_ready_or_not(client.clone()).await?;
        if !is_postgres_ready().matches_object(Some(&primary_pod)) {
            debug!("Primary pod is not ready, skipping setup_pgbouncer");
            return Ok(());
        }

        match setup_pgbouncer_function(cdb, ctx.clone()).await {
            Ok(_) => debug!(
                "Successfully created setup_pgbouncer function on instance {}",
                cdb.name_any()
            ),
            Err(e) => {
                warn!(
                    "Did not create setup_pgbouncer function, will requeue: {:?}",
                    e
                );
                return Err(Action::requeue(Duration::from_secs(30)));
            }
        }
        // Run the setup_pgbouncer function
        cdb.psql(
            "SELECT setup_pgbouncer();".to_string(),
            "postgres".to_string(),
            ctx.clone(),
        )
        .await?;
    } else {
        // If pooler is disabled and exists, delete
        let pooler = pooler_api.get(&name).await;
        if pooler.is_err() {
            debug!("Pooler {name} does not exist. Skipping deletion");
            return Ok(());
        } else {
            debug!("Found pooler {name} and pooler is disabled. Deleting Pooler {name}");
            let dp = DeleteParams::default();
            pooler_api.delete(&name, &dp).await.map_err(|e| {
                error!("Error deleting Pooler: {}", e);
                Action::requeue(Duration::from_secs(300))
            })?;
        }
    }

    Ok(())
}

// This function was created from the instructions that CNPG gives when you have to setup pgbouncer
// manually.  You can read more here: https://cloudnative-pg.io/documentation/1.20/connection_pooling/#authentication
const PGBOUNCER_SETUP_FUNCTION: &str = r#"
CREATE OR REPLACE FUNCTION setup_pgbouncer() RETURNS VOID LANGUAGE plpgsql AS $$
DECLARE
    db_name TEXT;
    db_list CURSOR FOR SELECT datname FROM pg_database WHERE datistemplate = false;
BEGIN
    -- Check if the role exists, if not create it
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'cnpg_pooler_pgbouncer') THEN
        EXECUTE 'CREATE ROLE cnpg_pooler_pgbouncer WITH LOGIN;';
    END IF;

    -- Iterate through all databases and set up permissions
    OPEN db_list;
    LOOP
        FETCH db_list INTO db_name;
        EXIT WHEN NOT FOUND;

        -- Check if the role has CONNECT permission on the database, if not grant it
        IF NOT EXISTS (
            SELECT 1
            FROM pg_database db
            JOIN pg_auth_members am ON db.datdba = am.roleid
            JOIN pg_roles r ON am.member = r.oid
            WHERE db.datname = db_name AND r.rolname = 'cnpg_pooler_pgbouncer'
        ) THEN
            EXECUTE format('GRANT CONNECT ON DATABASE %I TO cnpg_pooler_pgbouncer;', db_name);
        END IF;
    END LOOP;
    CLOSE db_list;

    -- Check if the function exists, if not create it
    IF NOT EXISTS (
        SELECT 1
        FROM pg_proc p
        JOIN pg_namespace n ON p.pronamespace = n.oid
        WHERE n.nspname = 'public' AND p.proname = 'user_search'
    ) THEN
        EXECUTE '
            CREATE OR REPLACE FUNCTION user_search(uname TEXT)
            RETURNS TABLE (usename name, passwd text)
            LANGUAGE sql SECURITY DEFINER AS
            ''SELECT usename, passwd FROM pg_shadow WHERE usename=$1;'';';
    END IF;

    -- Check if the role has EXECUTE permission on the function, if not grant it
    IF NOT EXISTS (
        SELECT 1
        FROM pg_proc p
        JOIN pg_namespace n ON p.pronamespace = n.oid
        JOIN pg_auth_members am ON p.proowner = am.roleid
        JOIN pg_roles r ON am.member = r.oid
        WHERE n.nspname = 'public' AND p.proname = 'user_search' AND r.rolname = 'cnpg_pooler_pgbouncer'
    ) THEN
        EXECUTE 'REVOKE ALL ON FUNCTION user_search(text) FROM public;';
        EXECUTE 'GRANT EXECUTE ON FUNCTION user_search(text) TO cnpg_pooler_pgbouncer;';
    END IF;
    
END;
$$;"#;

#[instrument(skip(coredb, ctx) fields(trace_id, instance_name = %coredb.name_any()))]
async fn setup_pgbouncer_function(
    coredb: &CoreDB,
    ctx: Arc<Context>,
) -> Result<PsqlOutput, Action> {
    // execute the PGBOUNCER_SETUP_FUNCTION to install/update the function
    // on the instance
    let query = coredb
        .psql(
            PGBOUNCER_SETUP_FUNCTION.to_string(),
            "postgres".to_string(),
            ctx.clone(),
        )
        .await?;
    Ok(query)
}

#[instrument(skip(cdb) fields(trace_id, instance_name = %cdb.name_any()))]
fn schedule_expression_from_cdb(cdb: &CoreDB) -> String {
    // Default to daily at midnight
    let default = "0 0 0 * * *".to_string();
    match &cdb.spec.backup.schedule {
        None => default,
        Some(expression) => {
            let mut terms = expression.split(' ').collect::<Vec<&str>>();
            if terms.len() == 5 {
                // pre-pend "0" to the vector
                let mut new_terms = vec!["0"];
                new_terms.extend(terms);
                terms = new_terms.clone();
            }
            if terms.len() != 6 {
                warn!("Invalid schedule expression, expected five or six terms. Setting as default. Found expression: '{}'", expression);
                return default;
            }
            // check that all terms are either parsable as int32 or "*"
            for term in &terms {
                if *term != "*" {
                    match term.parse::<i32>() {
                        Ok(_) => {}
                        Err(_) => {
                            warn!("Invalid schedule expression, only integers and '*' are accepted, setting as default. Found: {}", expression);
                            return default;
                        }
                    }
                }
            }
            terms.join(" ")
        }
    }
}

// Generate a ScheduledBackup
fn cnpg_scheduled_backup(cdb: &CoreDB) -> ScheduledBackup {
    let name = cdb.name_any();
    let namespace = cdb.namespace().unwrap();

    ScheduledBackup {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(namespace),
            ..ObjectMeta::default()
        },
        spec: ScheduledBackupSpec {
            backup_owner_reference: Some(ScheduledBackupBackupOwnerReference::Cluster),
            cluster: ScheduledBackupCluster { name },
            immediate: Some(true),
            schedule: schedule_expression_from_cdb(cdb),
            suspend: Some(false),
            ..ScheduledBackupSpec::default()
        },
        status: None,
    }
}

// Reconcile a SheduledBackup
#[instrument(skip(cdb, ctx), fields(trace_id, instance_name = %cdb.name_any()))]
pub async fn reconcile_cnpg_scheduled_backup(
    cdb: &CoreDB,
    ctx: Arc<Context>,
) -> Result<(), Action> {
    // check if the Cluster object exists on the cluster, if not then requeue
    if !does_cluster_exist(cdb, ctx.clone()).await? {
        warn!("Cluster does not exist, requeuing ScheduledBackup");
        return Err(Action::requeue(Duration::from_secs(30)));
    }

    let scheduledbackup = cnpg_scheduled_backup(cdb);
    let client = ctx.client.clone();
    let name = scheduledbackup
        .metadata
        .name
        .clone()
        .expect("ScheduledBackup should always have a name");
    let namespace = scheduledbackup
        .metadata
        .namespace
        .clone()
        .expect("ScheduledBackup should always have a namespace");
    let backup_api: Api<ScheduledBackup> = Api::namespaced(client.clone(), namespace.as_str());

    debug!("Patching ScheduledBackup");
    let ps = PatchParams::apply("cntrlr").force();
    let _o = backup_api
        .patch(&name, &ps, &Patch::Apply(&scheduledbackup))
        .await
        .map_err(|e| {
            error!("Error patching ScheduledBackup: {}", e);
            Action::requeue(Duration::from_secs(300))
        })?;
    debug!("Applied ScheduledBackup");
    Ok(())
}

// Lookup latestGeneratedNode from the Cluster Status and return the index number
#[instrument(skip(cdb, ctx), fields(trace_id))]
pub async fn get_latest_generated_node(
    cdb: &CoreDB,
    ctx: Arc<Context>,
    instance_name: &str,
) -> Result<Option<String>, Action> {
    let namespace = cdb.namespace().unwrap();
    let cluster: Api<Cluster> = Api::namespaced(ctx.client.clone(), &namespace);
    let co = cluster.get(instance_name).await;

    if let Ok(cluster_resource) = co {
        if let Some(status) = cluster_resource.status {
            if let Some(latest_generated_node) = status.latest_generated_node {
                debug!(
                    "The latestGeneratedNode for instance {}: {:?}",
                    instance_name, latest_generated_node
                );
                Ok(Some(latest_generated_node.to_string()))
            } else {
                error!(
                    "The latestGeneratedNode is not set in the Cluster Status for instance {}",
                    instance_name
                );
                Err(Action::requeue(Duration::from_secs(30)))
            }
        } else {
            error!("Instance Status is not set for instance {}", instance_name);
            Err(Action::requeue(Duration::from_secs(30)))
        }
    } else {
        info!(
            "Instance {} not found, possible new instance detected",
            instance_name
        );
        Ok(None)
    }
}

/// fenced_pods_initialized checks if fenced pods are initialized and retuns a bool or action in a
/// result
#[instrument(skip(cdb, ctx), fields(trace_id, instance_name = %cdb.name_any()))]
async fn fenced_pods_initialized(
    cdb: &CoreDB,
    ctx: Arc<Context>,
    pod_name: &str,
) -> Result<bool, Action> {
    let instance_name = cdb.name_any();
    let namespace = cdb.namespace().ok_or_else(|| {
        error!("Namespace is not set for CoreDB {}", instance_name);
        Action::requeue(Duration::from_secs(300))
    })?;

    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), &namespace);
    let po = pods.get(pod_name).await;

    match po {
        Ok(pod_resource) => {
            if let Some(status) = pod_resource.status {
                if let Some(conditions) = status.conditions {
                    let initialized_condition = conditions.iter().find(|condition| {
                        condition.type_ == "Initialized" && condition.status == "True"
                    });
                    return Ok(initialized_condition.is_some());
                }
            }
            error!(
                "Pod Status is not set for pod {} in instance {}",
                pod_name, instance_name
            );
            Err(Action::requeue(Duration::from_secs(10)))
        }
        Err(_) => {
            info!(
                "Pod {} not found, possible new pod detected for instance {}",
                pod_name, instance_name
            );
            Ok(false)
        }
    }
}

// get_fenced_instances_from_annotations returns a list of fenced instances from the annotations as
// a BTreeMap of String, String
#[instrument(fields(trace_id, annotations))]
pub fn get_fenced_instances_from_annotations(
    annotations: &BTreeMap<String, String>,
) -> Result<Option<Vec<String>>, serde_json::Error> {
    if let Some(fenced_instances) = annotations.get("cnpg.io/fencedInstances") {
        let fenced_instances: Vec<String> = serde_json::from_str(fenced_instances)?;
        Ok(Some(fenced_instances))
    } else {
        Ok(None)
    }
}

// get_fenced_nodes returns a list of nodes that are fenced only after all the pods are initialized
#[instrument(skip(cdb, ctx), fields(trace_id, instance_name = %cdb.name_any()))]
pub async fn get_fenced_pods(
    cdb: &CoreDB,
    ctx: Arc<Context>,
) -> Result<Option<Vec<String>>, Action> {
    let instance_name = cdb.metadata.name.as_deref().unwrap_or_default();
    let namespace = cdb.namespace().ok_or_else(|| {
        error!("Namespace is not set for CoreDB instance {}", instance_name);
        Action::requeue(Duration::from_secs(300))
    })?;

    let cluster: Api<Cluster> = Api::namespaced(ctx.client.clone(), &namespace);
    let co = cluster.get(instance_name).await.map_err(|e| {
        error!("Error getting cluster: {}", e);
        Action::requeue(Duration::from_secs(300))
    })?;

    // Directly bind the value from co to cluster_resource
    let cluster_resource = co;

    let annotations = match cluster_resource.metadata.annotations {
        Some(ann) => ann,
        None => {
            info!("Cluster Status for {} is not set", instance_name);
            return Ok(None);
        }
    };

    // Handle the Result returned by get_fenced_instances_from_annotations
    let fenced_instances = match get_fenced_instances_from_annotations(&annotations) {
        Ok(fi) => fi,
        Err(_) => {
            error!(
                "Error while parsing fenced instances for instance {}",
                instance_name
            );
            return Err(Action::requeue(Duration::from_secs(30)));
        }
    };

    // Check if fencedInstances annotation is present
    if let Some(fenced_instances) = fenced_instances {
        debug!(
            "Found fenced pods {:?} for instance {}",
            fenced_instances, instance_name
        );

        let primary_pod_name = cdb
            .primary_pod_cnpg_ready_or_not(ctx.client.clone())
            .await?
            .name_any();

        // Check if primary pod is initialized
        let is_primary_initialized =
            fenced_pods_initialized(cdb, ctx.clone(), &primary_pod_name).await?;
        if is_primary_initialized {
            debug!(
                "Primary pod {} is initialized for instance {}",
                primary_pod_name, instance_name
            );
            // Return early to get the primary pod unfenced first
            return Ok(Some(fenced_instances));
        }
        warn!(
            "Primary pod {} is not yet initialized for instance {}",
            primary_pod_name, instance_name
        );

        // Check if other fenced pods (excluding primary) are initialized
        for pod_name in &fenced_instances {
            if pod_name == &primary_pod_name {
                debug!(
                    "Primary pod {} is initialized for instance {}",
                    primary_pod_name, instance_name
                );
                continue;
            }

            let is_initialized = fenced_pods_initialized(cdb, ctx.clone(), pod_name).await?;
            if !is_initialized {
                info!(
                    "Pod {} in {} is not yet initialized. Will requeue, but work continues on primary.",
                    pod_name, instance_name
                );
                return Err(Action::requeue(Duration::from_secs(10)));
            }
        }

        Ok(Some(fenced_instances))
    } else {
        debug!(
            "The fencedInstances annotation for instance {} is not set in the Cluster Status",
            instance_name
        );
        Ok(None)
    }
}

// get_instance_replicas will look up cluster.spec.instances from Kubernetes and return i64 value
#[instrument(skip(cdb, ctx), fields(trace_id, instance_name = %cdb.name_any()))]
async fn get_instance_replicas(cdb: &CoreDB, ctx: Arc<Context>) -> Result<i64, Action> {
    let namespace = match cdb.namespace() {
        Some(ns) => ns,
        None => {
            error!(
                "Namespace is not set for CoreDB instance {}",
                cdb.name_any()
            );
            return Err(Action::requeue(Duration::from_secs(300)));
        }
    };

    let cluster: Api<Cluster> = Api::namespaced(ctx.client.clone(), &namespace);
    let co = cluster.get(&cdb.name_any()).await.map_err(|e| {
        info!(
            "Cluster is missing, instance {} maybe starting: {}",
            cdb.name_any(),
            e
        );
        Action::requeue(Duration::from_secs(30))
    })?;

    Ok(co.spec.instances)
}

// remove_pod_from_fenced_instances_annotation function will remove the pod name from the fencedInstances annotation
// and return the updated annotations as a BTreeMap
#[instrument(fields(trace_id, annotations, pod_name))]
fn remove_pod_from_fenced_instances_annotation(
    annotations: &BTreeMap<String, String>,
    pod_name: &str,
) -> Result<Option<BTreeMap<String, String>>, serde_json::Error> {
    if let Some(fenced_instances) = annotations.get("cnpg.io/fencedInstances") {
        let mut fenced_instances: Vec<String> = serde_json::from_str(fenced_instances)?;
        fenced_instances.retain(|x| x != pod_name);

        let mut updated_annotations = annotations.clone();
        if fenced_instances.is_empty() {
            updated_annotations.remove("cnpg.io/fencedInstances");
        } else {
            updated_annotations.insert(
                "cnpg.io/fencedInstances".to_string(),
                serde_json::to_string(&fenced_instances)?,
            );
        }

        Ok(Some(updated_annotations))
    } else {
        Ok(None)
    }
}

// unfence_pod function will remove the fencing annotation from the cluster object
#[instrument(skip(cdb, ctx), fields(trace_id, instance_name = %cdb.name_any(), pod_name))]
pub async fn unfence_pod(cdb: &CoreDB, ctx: Arc<Context>, pod_name: &str) -> Result<(), Action> {
    let instance_name = cdb.metadata.name.as_deref().unwrap_or_default();
    let namespace = match cdb.namespace() {
        Some(ns) => ns,
        None => {
            error!(
                "Namespace is not set for CoreDB for instance {}",
                instance_name
            );
            return Err(Action::requeue(Duration::from_secs(300)));
        }
    };

    let cluster: Api<Cluster> = Api::namespaced(ctx.client.clone(), &namespace);
    let co = cluster.get(instance_name).await.map_err(|e| {
        error!("Error getting cluster: {}", e);
        Action::requeue(Duration::from_secs(300))
    })?;

    // Directly bind the value from co to cluster_resource
    let mut cluster_resource = co;

    let annotations_clone = cluster_resource.metadata.annotations.clone();
    debug!(
        "Instance initial annotations for instance {}: {:?}",
        instance_name, annotations_clone
    );

    if let Some(annotations) = annotations_clone {
        // Use the remove_pod_from_fenced_instances function
        let updated_annotations =
            remove_pod_from_fenced_instances_annotation(&annotations, pod_name);

        if let Ok(Some(updated_annotations)) = updated_annotations {
            // Update the cluster object
            cluster_resource.metadata.annotations = Some(updated_annotations.clone());

            // Clear managedFields
            cluster_resource.metadata.managed_fields = None;

            // Patch the cluster object
            debug!("Patching Cluster resource for instance {}", instance_name);
            let ps = PatchParams::apply("cntrlr").force();
            let _o = cluster
                .patch(instance_name, &ps, &Patch::Apply(&cluster_resource))
                .await
                .map_err(|e| {
                    warn!(
                        "Issue patching annotation for instance {}, will requeue: {}",
                        instance_name, e
                    );
                    Action::requeue(Duration::from_secs(30))
                })?;
            debug!("Cluster annotations patched for instance {}", instance_name);
            Ok(())
        } else {
            debug!("The fencedInstances annotation is not set in the Cluster Status for instance {}. Removing the key.", instance_name);

            // Remove the "cnpg.io/fencedInstances" annotation
            let mut updated_annotations = annotations.clone();
            updated_annotations.remove("cnpg.io/fencedInstances");

            // Update the cluster object
            cluster_resource.metadata.annotations = if updated_annotations.is_empty() {
                None
            } else {
                Some(updated_annotations.clone())
            };

            // Clear managedFields
            cluster_resource.metadata.managed_fields = None;

            // Patch the cluster object
            debug!("Patch Cluster annotation for instance {}", instance_name);
            let ps = PatchParams::apply("cntrlr").force();
            let _o = cluster
                .patch(instance_name, &ps, &Patch::Apply(&cluster_resource))
                .await
                .map_err(|e| {
                    warn!(
                        "Issue patching annotation for instance {}, will requeue: {}",
                        instance_name, e
                    );
                    Action::requeue(Duration::from_secs(30))
                })?;
            debug!("Cluster annotations patched for instance {}", instance_name);
            Ok(())
        }
    } else {
        info!("Cluster Status is not set for {}", instance_name);
        Ok(())
    }
}

// generate_restore_destination_path function will generate the restore destination path from the backup
// object and return a string
#[instrument(fields(trace_id, path))]
fn generate_restore_destination_path(path: &str) -> String {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts.join("/")
}

// generate_s3_backup_credentials function will generate the s3 backup credentials from
// S3Credentials object and return a ClusterBackupBarmanObjectStoreS3Credentials object
#[instrument(fields(trace_id, creds))]
fn generate_s3_backup_credentials(
    creds: Option<&S3Credentials>,
) -> ClusterBackupBarmanObjectStoreS3Credentials {
    if let Some(creds) = creds {
        if creds.access_key_id.is_none() && creds.secret_access_key.is_none() {
            return ClusterBackupBarmanObjectStoreS3Credentials {
                inherit_from_iam_role: Some(true),
                ..Default::default()
            };
        }

        ClusterBackupBarmanObjectStoreS3Credentials {
            access_key_id: creds.access_key_id.as_ref().map(|id| {
                ClusterBackupBarmanObjectStoreS3CredentialsAccessKeyId {
                    key: id.key.clone(),
                    name: id.name.clone(),
                }
            }),
            inherit_from_iam_role: Some(false),
            region: creds.region.as_ref().map(|r| {
                ClusterBackupBarmanObjectStoreS3CredentialsRegion {
                    key: r.key.clone(),
                    name: r.name.clone(),
                }
            }),
            secret_access_key: creds.secret_access_key.as_ref().map(|key| {
                ClusterBackupBarmanObjectStoreS3CredentialsSecretAccessKey {
                    key: key.key.clone(),
                    name: key.name.clone(),
                }
            }),
            session_token: creds.session_token.as_ref().map(|token| {
                ClusterBackupBarmanObjectStoreS3CredentialsSessionToken {
                    key: token.key.clone(),
                    name: token.name.clone(),
                }
            }),
        }
    } else {
        ClusterBackupBarmanObjectStoreS3Credentials {
            inherit_from_iam_role: Some(true),
            ..Default::default()
        }
    }
}

// generate_s3_restore_credentials function will generate the s3 restore credentials from
// S3Credentials object and return a ClusterExternalClustersBarmanObjectStoreS3Credentials object
#[instrument(fields(trace_id, creds))]
fn generate_s3_restore_credentials(
    creds: Option<&S3Credentials>,
) -> ClusterExternalClustersBarmanObjectStoreS3Credentials {
    if let Some(creds) = creds {
        if creds.access_key_id.is_none() && creds.secret_access_key.is_none() {
            return ClusterExternalClustersBarmanObjectStoreS3Credentials {
                inherit_from_iam_role: Some(true),
                ..Default::default()
            };
        }

        ClusterExternalClustersBarmanObjectStoreS3Credentials {
            access_key_id: creds.access_key_id.as_ref().map(|id| {
                ClusterExternalClustersBarmanObjectStoreS3CredentialsAccessKeyId {
                    key: id.key.clone(),
                    name: id.name.clone(),
                }
            }),
            inherit_from_iam_role: Some(false),
            region: creds.region.as_ref().map(|r| {
                ClusterExternalClustersBarmanObjectStoreS3CredentialsRegion {
                    key: r.key.clone(),
                    name: r.name.clone(),
                }
            }),
            secret_access_key: creds.secret_access_key.as_ref().map(|key| {
                ClusterExternalClustersBarmanObjectStoreS3CredentialsSecretAccessKey {
                    key: key.key.clone(),
                    name: key.name.clone(),
                }
            }),
            session_token: creds.session_token.as_ref().map(|token| {
                ClusterExternalClustersBarmanObjectStoreS3CredentialsSessionToken {
                    key: token.key.clone(),
                    name: token.name.clone(),
                }
            }),
        }
    } else {
        ClusterExternalClustersBarmanObjectStoreS3Credentials {
            inherit_from_iam_role: Some(true),
            ..Default::default()
        }
    }
}

// is_restore_backup_running_pending_completed checks if a backup is running or
// pending or completed and returns a bool or action in a result
#[instrument(skip(cdb, ctx), fields(trace_id, instance_name = %cdb.name_any()))]
async fn is_restore_backup_running_pending_completed(
    cdb: &CoreDB,
    ctx: Arc<Context>,
) -> Result<bool, Action> {
    let instance_name = cdb.name_any();
    let namespace = cdb.namespace().ok_or_else(|| {
        error!(
            "Namespace is not set for CoreDB for instance {}",
            instance_name
        );
        Action::requeue(Duration::from_secs(300))
    })?;

    let backups_api: Api<Backup> = Api::namespaced(ctx.client.clone(), &namespace);
    let label_selector = format!(
        "cnpg.io/cluster={},cnpg.io/immediateBackup=true",
        instance_name
    );
    let lp = ListParams::default().labels(&label_selector);
    let backup_result = backups_api.list(&lp).await;

    match backup_result {
        Ok(backup_list) => {
            for backup_item in backup_list.items {
                if let Some(status) = &backup_item.status {
                    if status.phase.as_deref() == Some("running")
                        || status.phase.as_deref() == Some("pending")
                        || status.phase.as_deref() == Some("completed")
                    {
                        debug!(
                            "Backup for instance {} is in a {:?} state",
                            instance_name,
                            status.phase.as_deref()
                        );
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        }
        Err(e) => {
            error!("Error listing backups: {}", e);
            Err(Action::requeue(Duration::from_secs(300)))
        }
    }
}

// does_cluster_exist checks if a cluster exists and returns a bool or action to
// requeue in a result
#[instrument(skip(cdb, ctx), fields(trace_id, instance_name = %cdb.name_any()))]
async fn does_cluster_exist(cdb: &CoreDB, ctx: Arc<Context>) -> Result<bool, Action> {
    let instance_name = cdb.name_any();
    let namespace = cdb.namespace().ok_or_else(|| {
        error!(
            "Namespace is not set for CoreDB for instance {}",
            instance_name
        );
        Action::requeue(Duration::from_secs(300))
    })?;

    let cluster: Api<Cluster> = Api::namespaced(ctx.client.clone(), &namespace);
    let co = cluster.get(&instance_name).await;

    match co {
        Ok(_) => {
            debug!("Cluster {} exists", instance_name);
            Ok(true)
        }
        Err(e) => {
            error!("Error getting cluster: {}", e);
            Err(Action::requeue(Duration::from_secs(10)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn test_deserialize_cluster() {
        let json_str = r#"
        {
          "apiVersion": "postgresql.cnpg.io/v1",
          "kind": "Cluster",
          "metadata": {
            "annotations": {
              "tembo-pod-init.tembo.io/inject": "true"
            },
            "creationTimestamp": "2023-07-03T18:15:32Z",
            "generation": 1,
            "managedFields": [
              {
                "apiVersion": "postgresql.cnpg.io/v1",
                "fieldsType": "FieldsV1",
                "fieldsV1": {
                  "f:metadata": {
                    "f:annotations": {
                      "f:tembo-pod-init.tembo.io/inject": {}
                    },
                    "f:ownerReferences": {
                      "k:{\"uid\":\"d8efe1ff-b09a-43ca-a568-def96235e754\"}": {}
                    }
                  },
                  "f:spec": {
                    "f:affinity": {
                      "f:podAntiAffinityType": {},
                      "f:topologyKey": {}
                    },
                    "f:bootstrap": {
                      "f:pg_basebackup": {
                        "f:source": {}
                      }
                    },
                    "f:enableSuperuserAccess": {},
                    "f:externalClusters": {},
                    "f:failoverDelay": {},
                    "f:imageName": {},
                    "f:instances": {},
                    "f:logLevel": {},
                    "f:maxSyncReplicas": {},
                    "f:minSyncReplicas": {},
                    "f:monitoring": {
                      "f:customQueriesConfigMap": {},
                      "f:disableDefaultQueries": {},
                      "f:enablePodMonitor": {}
                    },
                    "f:postgresGID": {},
                    "f:postgresUID": {},
                    "f:postgresql": {
                      "f:parameters": {
                        "f:archive_mode": {},
                        "f:archive_timeout": {},
                        "f:dynamic_shared_memory_type": {},
                        "f:log_destination": {},
                        "f:log_directory": {},
                        "f:log_filename": {},
                        "f:log_rotation_age": {},
                        "f:log_rotation_size": {},
                        "f:log_truncate_on_rotation": {},
                        "f:logging_collector": {},
                        "f:max_parallel_workers": {},
                        "f:max_replication_slots": {},
                        "f:max_worker_processes": {},
                        "f:shared_memory_type": {},
                        "f:wal_keep_size": {},
                        "f:wal_receiver_timeout": {},
                        "f:wal_sender_timeout": {}
                      },
                      "f:syncReplicaElectionConstraint": {
                        "f:enabled": {}
                      }
                    },
                    "f:primaryUpdateMethod": {},
                    "f:primaryUpdateStrategy": {},
                    "f:startDelay": {},
                    "f:stopDelay": {},
                    "f:storage": {
                      "f:resizeInUseVolumes": {},
                      "f:size": {}
                    },
                    "f:superuserSecret": {
                      "f:name": {}
                    },
                    "f:switchoverDelay": {}
                  }
                },
                "manager": "cntrlr",
                "operation": "Apply",
                "time": "2023-07-03T18:15:32Z"
              },
              {
                "apiVersion": "postgresql.cnpg.io/v1",
                "fieldsType": "FieldsV1",
                "fieldsV1": {
                  "f:status": {
                    ".": {},
                    "f:certificates": {
                      ".": {},
                      "f:clientCASecret": {},
                      "f:expirations": {
                        ".": {},
                        "f:test-coredb-ca": {},
                        "f:test-coredb-replication": {},
                        "f:test-coredb-server": {}
                      },
                      "f:replicationTLSSecret": {},
                      "f:serverAltDNSNames": {},
                      "f:serverCASecret": {},
                      "f:serverTLSSecret": {}
                    },
                    "f:cloudNativePGCommitHash": {},
                    "f:cloudNativePGOperatorHash": {},
                    "f:conditions": {},
                    "f:configMapResourceVersion": {
                      ".": {},
                      "f:metrics": {
                        ".": {},
                        "f:cnpg-default-monitoring": {}
                      }
                    },
                    "f:healthyPVC": {},
                    "f:instanceNames": {},
                    "f:instances": {},
                    "f:instancesStatus": {
                      ".": {},
                      "f:failed": {}
                    },
                    "f:jobCount": {},
                    "f:latestGeneratedNode": {},
                    "f:managedRolesStatus": {},
                    "f:phase": {},
                    "f:phaseReason": {},
                    "f:poolerIntegrations": {
                      ".": {},
                      "f:pgBouncerIntegration": {}
                    },
                    "f:pvcCount": {},
                    "f:readService": {},
                    "f:secretsResourceVersion": {
                      ".": {},
                      "f:clientCaSecretVersion": {},
                      "f:replicationSecretVersion": {},
                      "f:serverCaSecretVersion": {},
                      "f:serverSecretVersion": {},
                      "f:superuserSecretVersion": {}
                    },
                    "f:targetPrimary": {},
                    "f:targetPrimaryTimestamp": {},
                    "f:topology": {
                      ".": {},
                      "f:instances": {
                        ".": {},
                        "f:test-coredb-1": {}
                      },
                      "f:successfullyExtracted": {}
                    },
                    "f:writeService": {}
                  }
                },
                "manager": "Go-http-client",
                "operation": "Update",
                "subresource": "status",
                "time": "2023-07-03T18:16:49Z"
              }
            ],
            "name": "test-coredb",
            "namespace": "default",
            "ownerReferences": [
              {
                "apiVersion": "coredb.io/v1alpha1",
                "controller": true,
                "kind": "CoreDB",
                "name": "test-coredb",
                "uid": "d8efe1ff-b09a-43ca-a568-def96235e754"
              }
            ],
            "resourceVersion": "7675",
            "uid": "7bfae8f4-bc86-481b-8f7c-7a7a659da265"
          },
          "spec": {
            "affinity": {
              "podAntiAffinityType": "preferred",
              "topologyKey": "topology.kubernetes.io/zone"
            },
            "bootstrap": {
              "pg_basebackup": {
                "database": "",
                "owner": "",
                "source": "coredb"
              }
            },
            "enableSuperuserAccess": true,
            "externalClusters": [
              {
                "connectionParameters": {
                  "host": "test-coredb",
                  "user": "postgres"
                },
                "name": "coredb",
                "password": {
                  "key": "password",
                  "name": "test-coredb-connection"
                }
              }
            ],
            "failoverDelay": 0,
            "imageName": "quay.io/tembo/tembo-pg-cnpg:15.3.0-1-3953e4e",
            "instances": 1,
            "logLevel": "info",
            "maxSyncReplicas": 0,
            "minSyncReplicas": 0,
            "monitoring": {
              "customQueriesConfigMap": [
                {
                  "key": "queries",
                  "name": "cnpg-default-monitoring"
                }
              ],
              "disableDefaultQueries": false,
              "enablePodMonitor": true
            },
            "postgresGID": 26,
            "postgresUID": 26,
            "postgresql": {
              "parameters": {
                "archive_mode": "on",
                "archive_timeout": "5min",
                "dynamic_shared_memory_type": "posix",
                "log_destination": "csvlog",
                "log_directory": "/controller/log",
                "log_filename": "postgres",
                "log_rotation_age": "0",
                "log_rotation_size": "0",
                "log_truncate_on_rotation": "false",
                "logging_collector": "on",
                "max_parallel_workers": "32",
                "max_replication_slots": "32",
                "max_worker_processes": "32",
                "shared_memory_type": "mmap",
                "shared_preload_libraries": "",
                "wal_keep_size": "512MB",
                "wal_receiver_timeout": "5s",
                "wal_sender_timeout": "5s"
              },
              "syncReplicaElectionConstraint": {
                "enabled": false
              }
            },
            "primaryUpdateMethod": "restart",
            "primaryUpdateStrategy": "unsupervised",
            "resources": {},
            "startDelay": 30,
            "stopDelay": 30,
            "storage": {
              "resizeInUseVolumes": true,
              "size": "1Gi"
            },
            "superuserSecret": {
              "name": "test-coredb-connection"
            },
            "switchoverDelay": 60
          },
          "status": {
            "certificates": {
              "clientCASecret": "test-coredb-ca",
              "expirations": {
                "test-coredb-ca": "2023-10-01 18:10:32 +0000 UTC",
                "test-coredb-replication": "2023-10-01 18:10:32 +0000 UTC",
                "test-coredb-server": "2023-10-01 18:10:32 +0000 UTC"
              },
              "replicationTLSSecret": "test-coredb-replication",
              "serverAltDNSNames": [
                "test-coredb-rw",
                "test-coredb-rw.default",
                "test-coredb-rw.default.svc",
                "test-coredb-r",
                "test-coredb-r.default",
                "test-coredb-r.default.svc",
                "test-coredb-ro",
                "test-coredb-ro.default",
                "test-coredb-ro.default.svc"
              ],
              "serverCASecret": "test-coredb-ca",
              "serverTLSSecret": "test-coredb-server"
            },
            "cloudNativePGCommitHash": "9bf74c9e",
            "cloudNativePGOperatorHash": "5d5f339b30506db0996606d61237dcf639c1e0d3009c0399e87e99cc7bc2caf0",
            "conditions": [
              {
                "lastTransitionTime": "2023-07-03T18:15:32Z",
                "message": "Cluster Is Not Ready",
                "reason": "ClusterIsNotReady",
                "status": "False",
                "type": "Ready"
              }
            ],
            "configMapResourceVersion": {
              "metrics": {
                "cnpg-default-monitoring": "7435"
              }
            },
            "healthyPVC": [
              "test-coredb-1"
            ],
            "instanceNames": [
              "test-coredb-1"
            ],
            "instances": 1,
            "instancesStatus": {
              "failed": [
                "test-coredb-1"
              ]
            },
            "jobCount": 1,
            "latestGeneratedNode": 1,
            "managedRolesStatus": {},
            "phase": "Setting up primary",
            "phaseReason": "Creating primary instance test-coredb-1",
            "poolerIntegrations": {
              "pgBouncerIntegration": {}
            },
            "pvcCount": 1,
            "readService": "test-coredb-r",
            "secretsResourceVersion": {
              "clientCaSecretVersion": "7409",
              "replicationSecretVersion": "7411",
              "serverCaSecretVersion": "7409",
              "serverSecretVersion": "7410",
              "superuserSecretVersion": "7107"
            },
            "targetPrimary": "test-coredb-1",
            "targetPrimaryTimestamp": "2023-07-03T18:15:32.464538Z",
            "topology": {
              "instances": {
                "test-coredb-1": {}
              },
              "successfullyExtracted": true
            },
            "writeService": "test-coredb-rw"
          }
        }

        "#;

        let _result: Cluster =
            serde_json::from_str(json_str).expect("Should be able to deserialize");
    }

    use serde_yaml::from_str;

    #[test]
    fn test_cnpg_scheduled_backup() {
        // Arrange
        let cdb_yaml = r#"
        apiVersion: coredb.io/v1alpha1
        kind: CoreDB
        metadata:
          name: test
          namespace: default
        spec:
          backup:
            destinationPath: s3://aws-s3-bucket/tembo/backup
            encryption: AES256
            retentionPolicy: "45"
            schedule: 55 7 * * *
          image: quay.io/tembo/tembo-pg-cnpg:15.3.0-5-48d489e 
          port: 5432
          postgresExporterEnabled: true
          postgresExporterImage: quay.io/prometheuscommunity/postgres-exporter:v0.12.1
          replicas: 1
          resources:
            limits:
              cpu: "1"
              memory: 0.5Gi
          serviceAccountTemplate:
            metadata:
              annotations:
                eks.amazonaws.com/role-arn: arn:aws:iam::012345678901:role/aws-iam-role-iam
          sharedirStorage: 1Gi
          stop: false
          storage: 1Gi
          uid: 999
        "#;
        let cdb: CoreDB = from_str(cdb_yaml).unwrap();
        let cfg = Config::default();

        let scheduled_backup: ScheduledBackup = cnpg_scheduled_backup(&cdb);
        let (backup, service_account_template) = cnpg_backup_configuration(&cdb, &cfg);

        // Assert to make sure that backup schedule is set
        assert_eq!(scheduled_backup.spec.schedule, "0 55 7 * * *".to_string());
        assert_eq!(
            backup.clone().unwrap().retention_policy.unwrap(),
            "45d".to_string()
        );

        // Assert to make sure that backup destination path is set
        assert_eq!(
            backup
                .unwrap()
                .barman_object_store
                .unwrap()
                .destination_path,
            "s3://aws-s3-bucket/tembo/backup".to_string()
        );

        // Assert to make sure that service account template is set
        assert_eq!(
            service_account_template
                .unwrap()
                .metadata
                .annotations
                .unwrap()
                .get("eks.amazonaws.com/role-arn")
                .unwrap(),
            "arn:aws:iam::012345678901:role/aws-iam-role-iam"
        );
    }

    #[test]
    fn test_get_fenced_instances_from_annotations() {
        // Annotation exists and is valid
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "cnpg.io/fencedInstances".to_string(),
            json!(["node1", "node2"]).to_string(),
        );

        let result = get_fenced_instances_from_annotations(&annotations).unwrap();
        assert_eq!(result, Some(vec!["node1".to_string(), "node2".to_string()]));

        // Annotation exists but is invalid
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "cnpg.io/fencedInstances".to_string(),
            "invalid_json".to_string(),
        );

        let result = get_fenced_instances_from_annotations(&annotations);
        assert!(result.is_err());

        //Annotation does not exist
        let annotations = BTreeMap::new();
        let result = get_fenced_instances_from_annotations(&annotations).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_remove_pod_from_fenced_instances_annotation() {
        let mut annotations = BTreeMap::new();
        annotations.insert(
            "cnpg.io/fencedInstances".to_string(),
            "[\"pod1\", \"pod2\"]".to_string(),
        );

        //"pod1" is in the list
        let result = remove_pod_from_fenced_instances_annotation(&annotations, "pod1")
            .unwrap()
            .unwrap();
        assert_eq!(result.get("cnpg.io/fencedInstances").unwrap(), "[\"pod2\"]");

        //"pod3" is not in the list
        let result = remove_pod_from_fenced_instances_annotation(&annotations, "pod3")
            .unwrap()
            .unwrap();
        let expected: Vec<String> = serde_json::from_str("[\"pod1\", \"pod2\"]").unwrap();
        let actual: Vec<String> =
            serde_json::from_str(result.get("cnpg.io/fencedInstances").unwrap()).unwrap();
        assert_eq!(actual, expected);

        //"cnpg.io/fencedInstances" is not present
        let empty_annotations = BTreeMap::new();
        let result =
            remove_pod_from_fenced_instances_annotation(&empty_annotations, "pod1").unwrap();
        assert!(result.is_none());

        //"cnpg.io/fencedInstances" contains invalid JSON
        let mut invalid_annotations = BTreeMap::new();
        invalid_annotations.insert(
            "cnpg.io/fencedInstances".to_string(),
            "invalid_json".to_string(),
        );
        let result = remove_pod_from_fenced_instances_annotation(&invalid_annotations, "pod1");
        assert!(result.is_err());
    }

    #[test]
    fn test_calculate_pods_to_fence() {
        let latest_generated_node = 3;
        let diff_instances = 2;
        let base_name = "instance";

        let result = calculate_pods_to_fence(latest_generated_node, diff_instances, base_name);
        let expected = vec!["instance-4", "instance-5"];

        assert_eq!(result, expected);
    }

    #[test]
    fn test_calculate_pods_to_fence_empty() {
        let latest_generated_node = 3;
        let diff_instances = 0;
        let base_name = "instance";

        let result = calculate_pods_to_fence(latest_generated_node, diff_instances, base_name);
        let expected: Vec<String> = vec![];

        assert_eq!(result, expected);
    }

    #[test]
    fn test_extend_with_fenced_pods() {
        let mut pod_names_to_fence = vec!["instance-1".to_string(), "instance-2".to_string()];
        let fenced_pods = Some(vec!["instance-3".to_string(), "instance-4".to_string()]);

        extend_with_fenced_pods(&mut pod_names_to_fence, fenced_pods);

        let expected = vec!["instance-1", "instance-2", "instance-3", "instance-4"];

        assert_eq!(pod_names_to_fence, expected);
    }

    #[test]
    fn test_extend_with_fenced_pods_none() {
        let mut pod_names_to_fence = vec!["instance-1".to_string(), "instance-2".to_string()];
        let fenced_pods: Option<Vec<String>> = None;

        extend_with_fenced_pods(&mut pod_names_to_fence, fenced_pods);

        let expected = vec!["instance-1", "instance-2"];

        assert_eq!(pod_names_to_fence, expected);
    }

    #[test]
    fn test_parse_cnpg_with_managed_roles_in_status() {
        let json_str = r#"
        {
          "apiVersion": "postgresql.cnpg.io/v1",
          "kind": "Cluster",
          "metadata": {
            "name": "test-coredb",
            "namespace": "default"
          },
          "spec": {
            "imageName": "quay.io/tembo/standard-cnpg:15.3.0-1-0c19c7e",
            "instances": 1,
            "managed": {
              "roles": [
                {
                  "connectionLimit": -1,
                  "ensure": "present",
                  "inRoles": [
                    "pg_read_all_data"
                  ],
                  "inherit": true,
                  "login": true,
                  "name": "readonly",
                  "passwordSecret": {
                    "name": "test-coredb-ro-password"
                  }
                }
              ]
            }
          },
          "status": {
            "managedRolesStatus": {
              "byStatus": {
                "not-managed": [
                  "app"
                ],
                "reconciled": [
                  "readonly"
                ],
                "reserved": [
                  "postgres"
                ]
              },
              "passwordStatus": {
                "readonly": {
                  "transactionID": 726
                }
              }
            }
          }
        }
        "#;
        let _result: Cluster =
            serde_json::from_str(json_str).expect("Should be able to deserialize");
    }

    #[test]
    fn test_generate_restore_destination_path() {
        // Define test cases
        let test_cases = [
            (
                "s3://cdb-plat-use1-dev-instance-backups/coredb/coredb/org-coredb-inst-test-testing-test-1",
                "s3://cdb-plat-use1-dev-instance-backups/coredb/coredb",
            ),
            ("s3://path/with/multiple/segments", "s3://path/with/multiple"),
            ("s3://short/path", "s3://short"),
            ("single_segment", ""),
        ];

        for (input, expected) in test_cases.iter() {
            assert_eq!(generate_restore_destination_path(input), *expected);
        }
    }

    #[test]
    fn test_basic_format() {
        let result = parse_target_time(Some("2023-09-26 21:15:42")).unwrap();
        assert_eq!(result, Some("2023-09-26 21:15:42".to_string()));
    }

    #[test]
    fn test_milliseconds_and_offset() {
        let result = parse_target_time(Some("2023-09-26 21:15:42.123456+02:00")).unwrap();
        assert_eq!(result, Some("2023-09-26 21:15:42.123456+02".to_string()));
    }

    #[test]
    fn test_rfc3339() {
        let result = parse_target_time(Some("2023-09-26T21:15:42Z")).unwrap();
        assert_eq!(result, Some("2023-09-26 21:15:42.000000+00".to_string())); // adjusted expected output
    }

    #[test]
    fn test_rfc3339_with_offset() {
        let result = parse_target_time(Some("2023-09-26T21:15:42+05:00")).unwrap();
        assert_eq!(result, Some("2023-09-26 21:15:42.000000+05".to_string())); // adjusted expected output
    }

    #[test]
    fn test_rfc3339micro() {
        let result = parse_target_time(Some("2023-09-26T21:15:42.123456+05:00")).unwrap();
        assert_eq!(result, Some("2023-09-26 21:15:42.123456+05".to_string())); // adjusted expected output
    }

    #[test]
    fn test_invalid_format() {
        let result = parse_target_time(Some("invalid-format"));
        assert!(result.is_err()); // check for error
    }

    #[test]
    fn test_cnpg_cluster_storage_class() {
        let cdb_storage_class_yaml = r#"
        apiVersion: coredb.io/v1alpha1
        kind: CoreDB
        metadata:
          name: test
          namespace: default
        spec:
          image: quay.io/tembo/tembo-pg-cnpg:15.3.0-5-48d489e 
          port: 5432
          postgresExporterEnabled: true
          postgresExporterImage: quay.io/prometheuscommunity/postgres-exporter:v0.12.1
          replicas: 1
          resources:
            limits:
              cpu: "1"
              memory: 0.5Gi
          serviceAccountTemplate:
            metadata:
              annotations:
                eks.amazonaws.com/role-arn: arn:aws:iam::012345678901:role/aws-iam-role-iam
          sharedirStorage: 1Gi
          stop: false
          storage: 1Gi
          storageClass: "gp3-enc"
          uid: 999
        "#;
        let cdb_storage_class: CoreDB = from_str(cdb_storage_class_yaml).unwrap();
        assert_eq!(
            cnpg_cluster_storage_class(&cdb_storage_class),
            Some("gp3-enc".to_string())
        );

        let cdb_no_storage_class_yaml = r#"
        apiVersion: coredb.io/v1alpha1
        kind: CoreDB
        metadata:
          name: test
          namespace: default
        spec:
          image: quay.io/tembo/tembo-pg-cnpg:15.3.0-5-48d489e 
          port: 5432
          postgresExporterEnabled: true
          postgresExporterImage: quay.io/prometheuscommunity/postgres-exporter:v0.12.1
          replicas: 1
          resources:
            limits:
              cpu: "1"
              memory: 0.5Gi
          serviceAccountTemplate:
            metadata:
              annotations:
                eks.amazonaws.com/role-arn: arn:aws:iam::012345678901:role/aws-iam-role-iam
          sharedirStorage: 1Gi
          stop: false
          storage: 1Gi
          uid: 999
        "#;
        let cdb_no_storage_class: CoreDB = from_str(cdb_no_storage_class_yaml).unwrap();
        assert_eq!(cnpg_cluster_storage_class(&cdb_no_storage_class), None);
    }
}
