use cached::{Cached, LruTtlCache};
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
    /// The author of messages in this message burst
    author: User,
    /// The messages sent. These all have the same hash.
    messages: Vec<Message>,
    /// Whether this message burst is considered spam. If true, messages
    /// with the same hash should be deleted immediately without waiting for
    /// any threshold to be crossed
    is_spam: bool,
}

#[derive(Clone)]
struct GuildInfo {
    name: String,
}

pub struct MessageHandler {
    guild_ids: Arc<Mutex<HashMap<GuildId, GuildInfo>>>,
    bursts: Arc<Mutex<LruTtlCache<MessageHash, Arc<Mutex<MessageBurst>>>>>,
}

/// Minimum message length to consider running through the spam detection heuristic
const AUTOBAN_MIN_MESSAGE_LENGTH: usize = 16;
/// How many consecutive messages need to be sent to consider them spam
const AUTOBAN_SPAM_MESSAGE_THRESHOLD: usize = 3;
/// How long users should be timed out in case of spam
const AUTOBAN_DURATION: TimeDelta = TimeDelta::days(2);

impl MessageHandler {
    pub fn new() -> Self {
        MessageHandler {
            guild_ids: Arc::new(Mutex::new(HashMap::new())),
            bursts: Arc::new(Mutex::new(
                LruTtlCache::builder()
                    .max_size(1024)
                    .ttl(Duration::from_mins(3))
                    .refresh_on_hit(true)
                    .build()
                    .unwrap(),
            )),
        }
    }

    /// Report a message to the cache. Returns a `MessageBurst` in case this message triggered a
    /// spam alert.
    async fn report_message(&self, message: &Message) -> Option<Arc<Mutex<MessageBurst>>> {
        // Compute message hash
        let mut hasher = Sha256::new();
        hasher.update(message.author.id.get().to_le_bytes());
        hasher.update(&message.content);
        hasher.update(message.attachments.len().to_le_bytes());
        for attachment in &message.attachments {
            hasher.update(&attachment.size.to_le_bytes());
        }
        let message_hash: MessageHash = hasher.finalize().as_array::<32>().unwrap().clone();

        let mut cache = self.bursts.lock().await;
        let cache_entry_arc = cache.cache_get_or_set_with(message_hash, || {
            Arc::new(Mutex::new(MessageBurst {
                author: message.author.clone(),
                messages: vec![],
                is_spam: false,
            }))
        });
        let mut cache_entry = cache_entry_arc.lock().await;

        cache_entry.messages.push(message.clone());

        if cache_entry.is_spam || cache_entry.messages.len() >= AUTOBAN_SPAM_MESSAGE_THRESHOLD {
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

        if message.guild_id.is_none() {
            // If the message is not in a server, ignore it
            return;
        };

        let Some(burst_arc) = self.report_message(&message).await else {
            // No messages need to be deleted
            return;
        };
        let mut burst = burst_arc.lock().await;

        ctx.set_activity(Some(ActivityData::custom("Deleting spam messages…")));

        if !burst.is_spam {
            burst.is_spam = true;

            warn!(
                "Burst detected: {} ({})",
                &burst.author.name, &burst.author.id
            );

            if let Some(message) = burst.messages.first() {
                warn!(
                    "Message content: << {} >> ({} attachment{})",
                    message.content,
                    message.attachments.len(),
                    if message.attachments.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
            }

            self.timeout_member_in_all_guilds(&ctx, &message.author.id)
                .await;
        } else {
            warn!(
                "Incoming message from an already triggered burst, will delete immediately: {} ({})",
                &burst.author.name, &burst.author.id
            );
        }

        Self::delete_all_messages(&ctx, burst).await;

        ctx.set_activity(None);
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("{} is connected!", ready.user.name);
    }
}
