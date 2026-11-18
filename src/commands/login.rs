use crate::client;
use crate::config;
use anyhow::Result;

pub async fn run(server: &str, email: &str, password: &str) -> Result<()> {
    println!("Logging in to {}...", server);

    let (_, response) = client::connect(server, email, password).await?;

    println!("Login successful!");
    println!("User: {} ({})", response.user.nickname, response.user.id);

    config::save_credentials(server, email, password)?;

    println!("Credentials saved to ~/crcli.toml");

    Ok(())
}
