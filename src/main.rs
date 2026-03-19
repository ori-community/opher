mod message_handler;

use crate::message_handler::MessageHandler;
use serenity::Client;
use serenity::all::GatewayIntents;
use std::env;
use tracing::error;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let token =
        env::var("DISCORD_TOKEN").expect("Expected environment variable DISCORD_TOKEN to be set");

    let intents =
        GatewayIntents::GUILD_MESSAGES | GatewayIntents::GUILDS | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&token, intents)
        .event_handler(MessageHandler::new())
        .await
        .expect("Error creating client");

    if let Err(why) = client.start().await {
        error!("Client error: {why:?}");
    }
}
