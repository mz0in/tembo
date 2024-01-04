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
pub struct ConnectionInfo {
    #[serde(rename = "host")]
    pub host: String,
    #[serde(rename = "password")]
    pub password: String,
    #[serde(
        rename = "pooler_host",
        default,
        with = "::serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub pooler_host: Option<Option<String>>,
    #[serde(rename = "port")]
    pub port: i32,
    #[serde(rename = "user")]
    pub user: String,
}

impl ConnectionInfo {
    pub fn new(host: String, password: String, port: i32, user: String) -> ConnectionInfo {
        ConnectionInfo {
            host,
            password,
            pooler_host: None,
            port,
            user,
        }
    }
}
