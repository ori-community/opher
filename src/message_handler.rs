use cached::{Cached, TimedSizedCache};
use chrono::TimeDelta;
use serenity::all::{
    ActivityData, Context, CreateMessage, EditMember, EventHandler, Guild, GuildId, Message, Ready,
    Timestamp, User, UserId,
};
use serenity::async_trait;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};
use tracing::{error, info, warn};

type MessageHash = [u8; 32];

#[derive(Clone)]
pub struct MessageBurst {
    author: User,
    messages: Vec<Message>,
}

#[derive(Clone)]
struct GuildInfo {
    name: String,
}

pub struct MessageHandler {
    guild_ids: Arc<Mutex<HashMap<GuildId, GuildInfo>>>,
    bursts: Arc<Mutex<TimedSizedCache<MessageHash, Arc<Mutex<MessageBurst>>>>>,
}

const AUTOBAN_MIN_MESSAGE_LENGTH: usize = 0;
const AUTOBAN_BURST: usize = 3;
const AUTOBAN_DURATION: TimeDelta = TimeDelta::seconds(30);

impl MessageHandler {
    pub fn new() -> Self {
        MessageHandler {
            guild_ids: Arc::new(Mutex::new(HashMap::new())),
            bursts: Arc::new(Mutex::new(
                TimedSizedCache::with_size_and_lifespan_and_refresh(
                    1024,
                    Duration::from_mins(3),
                    true,
                ),
            )),
        }
    }

    /// Report a message to the cache. Returns a `MessageBurst` in case this message triggered a
    /// spam alert.
    async fn report_message(&self, message: &Message) -> Option<Arc<Mutex<MessageBurst>>> {
        // Compute message hash
        let mut hasher = Sha256::new();
        hasher.update(message.author.id.get().to_le_bytes());
        hasher.update(message.content.clone());
        let message_hash: MessageHash = hasher.finalize().as_array::<32>().unwrap().clone();

        let mut cache = self.bursts.lock().await;
        let cache_entry_arc = cache.cache_get_or_set_with(message_hash, || {
            Arc::new(Mutex::new(MessageBurst {
                author: message.author.clone(),
                messages: vec![],
            }))
        });
        let mut cache_entry = cache_entry_arc.lock().await;

        cache_entry.messages.push(message.clone());

        if cache_entry.messages.len() >= AUTOBAN_BURST {
            Some(cache_entry_arc.clone())
        } else {
            None
        }
    }

    async fn timeout_member_in_all_guilds(&self, ctx: &Context, user_id: &UserId) {
        for (guild_id, guild_info) in self.guild_ids.lock().await.iter() {
            info!(
                "Timing out member {} in guild {} ({})",
                user_id, guild_id, guild_info.name
            );

            if let Ok(channels) = guild_id.channels(ctx).await {
                if let Some((channel_id, _)) = channels
                    .iter()
                    .find(|(_, channel)| channel.name == "opher-automod")
                {
                    if let Err(error) = channel_id
                        .send_message(
                            ctx,
                            CreateMessage::new()
                                .content(format!("Auto-Timeout triggered: <@{user_id}>")),
                        )
                        .await
                    {
                        error!("Error sending notification message: {}", error);
                    }
                } else {
                    warn!("Failed to find opher-automod channel in guild {}", guild_id);
                }
            } else {
                error!("Failed to get channels for guild {}", guild_id);
            }

            if let Err(error) = guild_id
                .edit_member(
                    ctx,
                    user_id,
                    EditMember::new().disable_communication_until_datetime(Timestamp::from(
                        Timestamp::now()
                            .checked_add_signed(AUTOBAN_DURATION)
                            .expect("Failed to get timestamp in 2 days"),
                    )),
                )
                .await
            {
                error!(
                    "Failed to timeout member in guild {} ({}): {}",
                    guild_info.name, guild_id, error
                );
            }
        }
    }

    async fn delete_all_messages(ctx: &Context, mut burst: MutexGuard<'_, MessageBurst>) {
        for message in &burst.messages {
            if let Err(error) = message.delete(ctx).await {
                error!("Failed to delete message: {}", error);
            }
        }

        burst.messages.clear();
    }
}

#[async_trait]
impl EventHandler for MessageHandler {
    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        info!("Discovered Guild {} ({})", guild.name, guild.id);
        self.guild_ids.lock().await.insert(
            guild.id,
            GuildInfo {
                name: guild.name.clone(),
            },
        );
    }

    async fn message(&self, ctx: Context, message: Message) {
        // Ignore other bots, even ourselves. We only serve humans here!
        // (at least ones that pretend to be humans...)
        if message.author.bot {
            return;
        }

        // Ignore short messages with no attachments and no embeds
        if message.attachments.is_empty()
            && message.embeds.is_empty()
            && message.content.len() <= AUTOBAN_MIN_MESSAGE_LENGTH
        {
            return;
        }

        let Some(_) = message.guild_id else {
            // If the message is not in a server, ignore it
            return;
        };

        let Some(burst_arc) = self.report_message(&message).await else {
            // No messages need to be deleted
            return;
        };
        let burst = burst_arc.lock().await;

        warn!(
            "Burst detected: {} ({})",
            &burst.author.name, &burst.author.id
        );

        ctx.set_activity(Some(ActivityData::custom("Deleting spam messages…")));

        self.timeout_member_in_all_guilds(&ctx, &message.author.id)
            .await;
        Self::delete_all_messages(&ctx, burst).await;

        ctx.set_activity(None);
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("{} is connected!", ready.user.name);
    }
}
