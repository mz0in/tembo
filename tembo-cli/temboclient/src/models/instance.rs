/*
 * Tembo Cloud
 *
 * Platform API for Tembo Cloud             </br>             </br>             To find a Tembo Data API, please find it here:             </br>             </br>             [AWS US East 1](https://api.data-1.use1.tembo.io/swagger-ui/)
 *
 * The version of the OpenAPI document: v1.0.0
 *
 * Generated by: https://openapi-generator.tech
 */

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Instance {
    #[serde(
        rename = "app_services",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub app_services: Option<Option<Vec<crate::models::AppType>>>,
    #[serde(
        rename = "connection_info",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub connection_info: Option<Option<Box<crate::models::ConnectionInfo>>>,
    #[serde(
        rename = "connection_pooler",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub connection_pooler: Option<Option<Box<crate::models::ConnectionPooler>>>,
    #[serde(rename = "cpu")]
    pub cpu: crate::models::Cpu,
    #[serde(rename = "created_at", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(rename = "environment")]
    pub environment: crate::models::Environment,
    #[serde(
        rename = "extensions",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub extensions: Option<Option<Vec<crate::models::ExtensionStatus>>>,
    #[serde(
        rename = "extra_domains_rw",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub extra_domains_rw: Option<Option<Vec<String>>>,
    #[serde(
        rename = "first_recoverability_time",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub first_recoverability_time: Option<Option<String>>,
    #[serde(rename = "instance_id")]
    pub instance_id: String,
    #[serde(rename = "instance_name")]
    pub instance_name: String,
    #[serde(
        rename = "ip_allow_list",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub ip_allow_list: Option<Option<Vec<String>>>,
    #[serde(rename = "last_updated_at", skip_serializing_if = "Option::is_none")]
    pub last_updated_at: Option<String>,
    #[serde(rename = "memory")]
    pub memory: crate::models::Memory,
    #[serde(rename = "organization_id")]
    pub organization_id: String,
    #[serde(rename = "organization_name")]
    pub organization_name: String,
    #[serde(
        rename = "postgres_configs",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub postgres_configs: Option<Option<Vec<crate::models::PgConfig>>>,
    #[serde(rename = "replicas")]
    pub replicas: i32,
    #[serde(
        rename = "runtime_config",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub runtime_config: Option<Option<Vec<crate::models::PgConfig>>>,
    #[serde(rename = "stack_type")]
    pub stack_type: crate::models::StackType,
    #[serde(rename = "state")]
    pub state: crate::models::State,
    #[serde(rename = "storage")]
    pub storage: crate::models::Storage,
    #[serde(
        rename = "trunk_installs",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub trunk_installs: Option<Option<Vec<crate::models::TrunkInstallStatus>>>,
}

impl Instance {
    pub fn new(
        cpu: crate::models::Cpu,
        environment: crate::models::Environment,
        instance_id: String,
        instance_name: String,
        memory: crate::models::Memory,
        organization_id: String,
        organization_name: String,
        replicas: i32,
        stack_type: crate::models::StackType,
        state: crate::models::State,
        storage: crate::models::Storage,
    ) -> Instance {
        Instance {
            app_services: None,
            connection_info: None,
            connection_pooler: None,
            cpu,
            created_at: None,
            environment,
            extensions: None,
            extra_domains_rw: None,
            first_recoverability_time: None,
            instance_id,
            instance_name,
            ip_allow_list: None,
            last_updated_at: None,
            memory,
            organization_id,
            organization_name,
            postgres_configs: None,
            replicas,
            runtime_config: None,
            stack_type,
            state,
            storage,
            trunk_installs: None,
        }
    }
}
