mod aichat;
use aichat::AiChat;

mod role;
use role::RoleDetails;

mod defaults;
use defaults::DEFAULT_CONFIG;

use clap::Parser;
use headjack::*;
use lazy_static::lazy_static;
use matrix_sdk::{
    media::{MediaFileHandle, MediaFormat, MediaRequest},
    room::MessagesOptions,
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::{
            receipt::ReceiptThread::Unthreaded,
            room::message::{
                AddMentions, ForwardThread, MessageType, OriginalSyncRoomMessageEvent,
                RoomMessageEventContent,
            },
        },
        OwnedUserId,
    },
    Room, RoomMemberships, RoomState,
};
use regex::Regex;
use serde::Deserialize;
use std::format;
use std::{collections::HashMap, fs::File, io::Read, path::PathBuf, sync::Mutex};
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct ChazArgs {
    /// path to config file
    #[arg(short, long)]
    config: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    homeserver_url: String,
    username: String,
    /// Optionally specify the password, if not set it will be asked for on cmd line
    password: Option<String>,
    /// Allow list of which accounts we will respond to
    allow_list: Option<String>,
    /// Per-account message limit while the bot is running
    message_limit: Option<u64>,
    /// Room size limit to respond to
    room_size_limit: Option<u64>,
    /// Set the state directory for chaz
    /// Defaults to $XDG_STATE_HOME/chaz
    state_dir: Option<String>,
    /// Set the config directory for aichat
    /// Allows for multiple instances setups of aichat
    aichat_config_dir: Option<String>,
    /// Model to use for summarizing chats
    /// Used for setting the room name/topic
    chat_summary_model: Option<String>,
    /// Default role
    role: Option<String>,
    /// Definitions of roles
    roles: Option<Vec<RoleDetails>>,
}

lazy_static! {
    /// Holds the config for the bot
    static ref GLOBAL_CONFIG: Mutex<Option<Config>> = Mutex::new(None);

    /// Count of the global messages per user
    static ref GLOBAL_MESSAGES: Mutex<HashMap<String, u64>> = Mutex::new(HashMap::new());
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Read in the config file
    let args = ChazArgs::parse();
    let mut file = File::open(args.config)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    let config: Config = serde_yaml::from_str(&contents)?;
    *GLOBAL_CONFIG.lock().unwrap() = Some(config.clone());

    // The config file is read, now we can start the bot
    let mut bot = Bot::new(BotConfig {
        login: Login {
            homeserver_url: config.homeserver_url,
            username: config.username.clone(),
            password: config.password,
        },
        name: Some(config.username.clone()),
        allow_list: config.allow_list,
        state_dir: config.state_dir,
    })
    .await;

    if let Err(e) = bot.login().await {
        error!("Error logging in: {e}");
    }

    // React to invites.
    // We set this up before the initial sync so that we join rooms
    // even if they were invited before the bot was started.
    bot.join_rooms();

    info!("The client is ready! Listening to new messages…");

    // The party command is from the matrix-rust-sdk examples
    // Keeping it as an easter egg
    bot.register_text_command("party", None, |_, _, room| async move {
        let content = RoomMessageEventContent::notice_plain(".🎉🎊🥳 let's PARTY!! 🥳🎊🎉");
        room.send(content).await.unwrap();
        Ok(())
    })
    .await;

    // print context with role and examples included
    // we don't expose it because one might want to avoid spoiling the role prompt
    // (full exposition can kind of ruin the magic of a quirky character)
    bot.register_text_command("fullcontext", None, |_, _, room| async move {
        let (mut context, _, _, _) = get_context(&room).await.unwrap();
        context = add_role(&context);
        context.insert_str(0, ".fullcontext:\n");
        let content = RoomMessageEventContent::notice_plain(context);
        room.send(content).await.unwrap();
        Ok(())
    })
    .await;

    // print context, exluding role and examples
    bot.register_text_command(
        "print",
        "Print the conversation".to_string(),
        |_, _, room| async move {
            let (mut context, _, _, _) = get_context(&room).await.unwrap();
            context.insert_str(0, ".context:\n");
            let content = RoomMessageEventContent::notice_plain(context);
            room.send(content).await.unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "send",
        "<message> - Send this message without context".to_string(),
        |sender, text, room| async move {
            if rate_limit(&room, &sender).await {
                return Ok(());
            }
            let input = text.trim_start_matches(".send").trim();

            // But we do need to read the context to figure out the model to use
            let (_, model, _, _) = get_context(&room).await.unwrap();

            info!(
                "Request: {} - {}",
                sender.as_str(),
                input.replace('\n', " ")
            );
            if let Ok(result) = get_backend().execute(&model, input.to_string(), Vec::new()) {
                // Add the prefix ".response:\n" to the result
                // That way we can identify our own responses and ignore them for context
                info!(
                    "Response: {} - {}",
                    sender.as_str(),
                    result.replace('\n', " ")
                );
                let result = format!(".response:\n{}", result);
                let content = RoomMessageEventContent::notice_plain(result);

                room.send(content).await.unwrap();
            }
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "model",
        "<model> - Select the model to use".to_string(),
        model,
    )
    .await;

    bot.register_text_command("list", "List available models".to_string(), list_models)
        .await;

    bot.register_text_command(
        "clear",
        "Ignore all messages before this point".to_string(),
        |_, _, room| async move {
            room.send(RoomMessageEventContent::notice_plain(
                ".clear: All messages before this will be ignored",
            ))
            .await
            .unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "leave",
        "Leave the room".to_string(),
        |_, _, room| async move {
            room.send(RoomMessageEventContent::notice_plain(
                ".leave: Leaving the room",
            ))
            .await
            .unwrap();
            room.leave().await.unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "lurk",
        "Do not respond (does not affect notices)".to_string(),
        |_, _, room| async move {
            room.send(RoomMessageEventContent::notice_plain(
                ".lurk: Will not engage in conversation",
            ))
            .await
            .unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "nolurk",
        "Stop lurking".to_string(),
        |_, _, room| async move {
            room.send(RoomMessageEventContent::notice_plain(
                ".lurk: Will respond normally",
            ))
            .await
            .unwrap();
            Ok(())
        },
    )
    .await;

    bot.register_text_command(
        "rename",
        "Rename the room and set the topic based on the chat content".to_string(),
        rename,
    )
    .await;

    // FIXME: need access to event id, so we can't use `Bot::register_text_handler`
    register_text_handler(&bot, |event, room: Room| async move {
        let sender = event.sender.clone();
        room.send_single_receipt(ReceiptType::Read, Unthreaded, event.event_id.to_owned())
            .await
            .unwrap();
        if rate_limit(&room, &sender).await {
            Ok("rate limited".to_string())
        } else if sender == room.client().user_id().unwrap().as_str() {
            Ok("not responding to myself".to_string())
        } else if let Ok((context, model, lurk, media)) = get_context(&room).await {
            if !lurk.unwrap_or(false) {
                // If it's not a command, we should send the full context without commands to the server
                let mut context = add_role(&context);
                // Append "ASSISTANT: " to the context string to indicate the assistant is speaking
                context.push_str("ASSISTANT: ");

                info!(
                    "Request: {} - {}",
                    sender.as_str(),
                    context.replace('\n', " ")
                );
                match get_backend().execute(&model, context, media) {
                    Ok(stdout) => {
                        info!("Response: {}", stdout.replace('\n', " "));
                        room.send(RoomMessageEventContent::text_plain(stdout).make_reply_to(
                            &event.into_full_event(room.room_id().to_owned()),
                            ForwardThread::No,
                            AddMentions::No,
                        ))
                        .await
                        .unwrap();
                        Ok("responded".to_string())
                    }
                    Err(stderr) => {
                        error!("Error: {}", stderr.replace('\n', " "));
                        room.send(RoomMessageEventContent::notice_plain(format!(
                            ".error: {}",
                            stderr.replace('\n', " ")
                        )))
                        .await
                        .unwrap();
                        Err("error: {stderr}".to_string())
                    }
                }
            } else {
                Ok("lurking".to_string())
            }
        } else {
            Err("could not get context".to_string())
        }
    });

    // Syncs to the current state
    if let Err(e) = bot.sync().await {
        error!("Error syncing: {e}");
    }

    // Run the bot, this should never return except on error
    if let Err(e) = bot.run().await {
        error!("Error running bot: {e}");
    }

    Ok(())
}

// modified Bot::register_text_handler which gives access to event_id
// (unfortunately we can't enforce the allowlist - `is_allowed` is private)
pub fn register_text_handler<F, Fut>(bot: &Bot, callback: F)
where
    F: FnOnce(OriginalSyncRoomMessageEvent, Room) -> Fut + Send + 'static + Clone + Sync,
    Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
{
    let client = bot.client();
    client.add_event_handler(
        move |event: OriginalSyncRoomMessageEvent, room: Room| async move {
            // Ignore messages from rooms we're not in
            if room.state() != RoomState::Joined {
                return;
            }
            let MessageType::Text(text_content) = &event.content.msgtype.to_owned() else {
                return;
            };
            let body = text_content.body.trim_start();
            if is_command(body) {
                return;
            }
            match callback(event, room).await {
                Err(e) => {
                    error!("Error responding to: {}\nError: {:?}", body, e);
                }
                Ok(res) => info!(res),
            }
        },
    );
}

/// Prepend the role defined in the global config
fn add_role(context: &str) -> String {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    role::prepend_role(
        context.to_string(),
        config.role.clone(),
        config.roles.clone(),
        DEFAULT_CONFIG.roles.clone(),
    )
}

/// Rate limit the user to a set number of messages
/// Returns true if the user is being rate limited
async fn rate_limit(room: &Room, sender: &OwnedUserId) -> bool {
    let room_size = room
        .members(RoomMemberships::ACTIVE)
        .await
        .unwrap_or(Vec::new())
        .len();
    let message_limit = GLOBAL_CONFIG
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .message_limit
        .unwrap_or(u64::max_value());
    let room_size_limit = GLOBAL_CONFIG
        .lock()
        .unwrap()
        .clone()
        .unwrap()
        .room_size_limit
        .unwrap_or(u64::max_value());
    let count = {
        let mut messages = GLOBAL_MESSAGES.lock().unwrap();
        let count = match messages.get_mut(sender.as_str()) {
            Some(count) => count,
            None => {
                // Insert the user with a val of 0 and return a mutable reference to the value
                messages.insert(sender.as_str().to_string(), 0);
                messages.get_mut(sender.as_str()).unwrap()
            }
        };
        // If the room is too big we will silently ignore the message
        // This is to prevent the bot from spamming large rooms
        if room_size as u64 > room_size_limit {
            return true;
        }
        if *count < message_limit {
            *count += 1;
            return false;
        }
        *count
    };
    error!("User {} has sent {} messages", sender, count);
    room.send(RoomMessageEventContent::notice_plain(format!(
        ".error: you have used up your message limit of {} messages.",
        message_limit
    )))
    .await
    .unwrap();
    true
}

/// List the available models
async fn list_models(_: OwnedUserId, _: String, room: Room) -> Result<(), ()> {
    let (_, current_model, _, _) = get_context(&room).await.unwrap();
    let response = format!(
        ".models:\n\ncurrent: {}\n\nAvailable Models:\n{}",
        current_model.unwrap_or(get_backend().default_model()),
        get_backend().list_models().join("\n")
    );
    room.send(RoomMessageEventContent::notice_plain(response))
        .await
        .unwrap();
    Ok(())
}

async fn model(sender: OwnedUserId, text: String, room: Room) -> Result<(), ()> {
    // Verify the command is fine
    // Get the second word in the command
    let model = text.split_whitespace().nth(1);
    if let Some(model) = model {
        let models = get_backend().list_models();
        if models.contains(&model.to_string()) {
            // Set the model
            let response = format!(".model: Set to \"{}\"", model);
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        } else {
            let response = format!(
                ".error: Model \"{}\" not found.\n\nAvailable models:\n{}",
                model,
                models.join("\n")
            );
            room.send(RoomMessageEventContent::notice_plain(response))
                .await
                .unwrap();
        }
    } else {
        list_models(sender, text, room).await?;
    }
    Ok(())
}

async fn rename(sender: OwnedUserId, _: String, room: Room) -> Result<(), ()> {
    if rate_limit(&room, &sender).await {
        return Ok(());
    }
    if let Ok((context, _, _, _)) = get_context(&room).await {
        let title_prompt= [
                            &context,
                            "\nUSER: Summarize this conversation in less than 20 characters to use as the title of this conversation. ",
                            "The output should be a single line of text describing the conversation. ",
                            "Do not output anything except for the summary text. ",
                            "Only the first 20 characters will be used. ",
                            "\nASSISTANT: ",
                        ].join("");
        let model = get_chat_summary_model();

        info!(
            "Request: {} - {}",
            sender.as_str(),
            title_prompt.replace('\n', " ")
        );
        let response = get_backend().execute(&model, title_prompt, Vec::new());
        if let Ok(result) = response {
            info!(
                "Response: {} - {}",
                sender.as_str(),
                result.replace('\n', " ")
            );
            let result = clean_summary_response(&result, None);
            if room.set_name(result).await.is_err() {
                room.send(RoomMessageEventContent::notice_plain(
                    ".error: I don't have permission to rename the room",
                ))
                .await
                .unwrap();

                // If we can't set the name, we can't set the topic either
                return Ok(());
            }
        }

        let topic_prompt = [
            &context,
            "\nUSER: Summarize this conversation in less than 50 characters. ",
            "Do not output anything except for the summary text. ",
            "Do not include any commentary or context, only the summary. ",
            "\nASSISTANT: ",
        ]
        .join("");

        info!(
            "Request: {} - {}",
            sender.as_str(),
            topic_prompt.replace('\n', " ")
        );
        let response = get_backend().execute(&model, topic_prompt, Vec::new());
        if let Ok(result) = response {
            info!(
                "Response: {} - {}",
                sender.as_str(),
                result.replace('\n', " ")
            );
            let result = clean_summary_response(&result, None);
            if room.set_room_topic(&result).await.is_err() {
                room.send(RoomMessageEventContent::notice_plain(
                    ".error: I don't have permission to set the topic",
                ))
                .await
                .unwrap();
            }
        }
    }
    Ok(())
}

/// Returns the backend based on the global config
fn get_backend() -> AiChat {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    AiChat::new("aichat".to_string(), config.aichat_config_dir.clone())
}

/// Try to clean up the response from the model containing a summary
/// Sometimes the models will return extra info, so we want to clean it if possible
fn clean_summary_response(response: &str, max_length: Option<usize>) -> String {
    let response = {
        // Try to clean the response
        // Should look for the first quoted string
        let re = Regex::new(r#""([^"]*)""#).unwrap();
        // If there are any matches, return the first one
        if let Some(caps) = re.captures(response) {
            caps.get(1).map_or("", |m| m.as_str())
        } else {
            response
        }
    };
    if let Some(max_length) = max_length {
        return response.chars().take(max_length).collect::<String>();
    }
    response.to_string()
}

/// Get the chat summary model from the global config
fn get_chat_summary_model() -> Option<String> {
    let config = GLOBAL_CONFIG.lock().unwrap().clone().unwrap();
    config.chat_summary_model
}

/// Gets the context of the current conversation
/// Returns a model if it was ever entered
async fn get_context(
    room: &Room,
) -> Result<(String, Option<String>, Option<bool>, Vec<MediaFileHandle>), ()> {
    // Read all the messages in the room, place them into a single string, and print them out
    let mut messages = Vec::new();

    let mut options = MessagesOptions::backward();
    let mut model_response = None;
    let mut lurk = None;
    let mut media = Vec::new();

    'outer: while let Ok(batch) = room.messages(options).await {
        // This assumes that the messages are in reverse order
        for message in batch.chunk {
            if let Some((sender, content)) = message
                .event
                .get_field::<String>("sender")
                .unwrap_or(None)
                .zip(
                    message
                        .event
                        .get_field::<RoomMessageEventContent>("content")
                        .unwrap_or(None),
                )
            {
                match &content.msgtype {
                    MessageType::Audio(audio_content) => {
                        messages.push(format!("USER sent an audio file: {}\n", audio_content.body));
                    }
                    MessageType::Emote(emote_content) => {
                        // USER sent an emote: sends hearts 💝
                        messages.push(format!("USER sent an emote: {}\n", emote_content.body));
                    }
                    MessageType::File(file_content) => {
                        messages.push(format!("USER sent a file: {}\n", file_content.body));
                        let request = MediaRequest {
                            source: file_content.source.clone(),
                            format: MediaFormat::File,
                        };
                        let mime = file_content
                            .info
                            .as_ref()
                            .unwrap()
                            .mimetype
                            .clone()
                            .unwrap()
                            .parse()
                            .unwrap();
                        let x = room
                            .client()
                            .media()
                            .get_media_file(&request, None, &mime, true, None)
                            .await
                            .unwrap();
                        media.insert(0, x);
                    }
                    MessageType::Image(image_content) => {
                        messages.push(format!("USER sent an image: {}\n", image_content.body));
                        let request = MediaRequest {
                            source: image_content.source.clone(),
                            format: MediaFormat::File,
                        };
                        let mime = image_content
                            .info
                            .as_ref()
                            .unwrap()
                            .mimetype
                            .clone()
                            .unwrap()
                            .parse()
                            .unwrap();
                        let x = room
                            .client()
                            .media()
                            .get_media_file(&request, None, &mime, true, None)
                            .await
                            .unwrap();
                        media.insert(0, x);
                    }
                    MessageType::Location(location_content) => {
                        messages.push(format!(
                            "USER sent their location: {}\n",
                            location_content.body
                        ));
                    }
                    MessageType::Notice(notice_content) => {
                        if sender != room.client().user_id().unwrap().as_str() {
                            messages.push(format!("USER sent a notice: {}\n", notice_content.body));
                        }
                    }
                    MessageType::ServerNotice(text_content) => {
                        messages.push(format!("SERVER: {}\n", text_content.body));
                    }
                    MessageType::Text(text_content) => {
                        if is_command(&text_content.body) {
                            // if the message is a valid model command, set the model
                            if text_content.body.starts_with(".model") && model_response.is_none() {
                                let model = text_content.body.split_whitespace().nth(1);
                                if let Some(model) = model {
                                    // Add the config_dir from the global config
                                    let models = get_backend().list_models();
                                    if models.contains(&model.to_string()) {
                                        model_response = Some(model.to_string());
                                    }
                                }
                            } else if text_content.body.starts_with(".nolurk") {
                                lurk = Some(false);
                            } else if text_content.body.starts_with(".lurk") && lurk.is_none() {
                                lurk = Some(true);
                            } else if text_content.body.starts_with(".clear") {
                                // if the message was a clear command, we are finished
                                break 'outer;
                            }
                        } else if !lurk.unwrap_or(false) {
                            // Push the sender and message to the front of the string
                            if room
                                .client()
                                .user_id()
                                .is_some_and(|uid| sender == uid.as_str())
                            {
                                // If the sender is the bot, prefix the message with "ASSISTANT: "
                                messages.push(format!("ASSISTANT: {}\n", text_content.body));
                            } else {
                                // Otherwise, prefix the message with "USER: "
                                messages.push(format!("USER: {}\n", text_content.body));
                            }
                        }
                    }
                    // not useful information
                    MessageType::VerificationRequest(_) => {}
                    MessageType::Video(video_content) => {
                        messages.push(format!("USER sent a video file: {}\n", video_content.body));
                    }
                    MessageType::_Custom(_) => {
                        messages.push(format!(
                            "USER sent a message of type {}: {}\n",
                            content.msgtype(),
                            content.body()
                        ));
                    }
                    x => {
                        warn!("Unhandled message type: {:#?}", x);
                    }
                };
            }
        }
        if let Some(token) = batch.end {
            options = MessagesOptions::backward().from(Some(token.as_str()));
        } else {
            break;
        }
    }
    // Append the messages into a string with newlines in between, in reverse order
    Ok((
        messages.into_iter().rev().collect::<String>(),
        model_response,
        lurk,
        media,
    ))
}
