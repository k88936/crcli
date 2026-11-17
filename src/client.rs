use anyhow::Result;
use cloudreve_sdk_api::api::user::UserApi;
use cloudreve_sdk_api::models::user::LoginResponse;
use cloudreve_sdk_api::{Client, ClientConfig};

const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Create a client and install the tokens returned by the password login.
pub async fn connect(server: &str, email: &str, password: &str) -> Result<(Client, LoginResponse)> {
    let client = Client::new(ClientConfig::new(server).with_user_agent(USER_AGENT));

    let response = client.login(email, password).await?;
    client.set_tokens_with_expiry(&response.token).await?;

    Ok((client, response))
}
