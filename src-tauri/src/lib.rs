use rand::Rng;
use argon2::{Argon2, Params, Version};
use tokio::sync::Mutex;
use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use lazy_static::lazy_static;
use nostr_sdk::prelude::*;
use once_cell::sync::OnceCell;

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

/// # Trusted Relay
/// 
/// The 'Trusted Relay' handles events that MAY have a small amount of public-facing metadata attached (i.e: Expiration tags).
/// 
/// This relay may be used for events like Typing Indicators, Key Exchanges (forward-secrecy setup) and more.
static TRUSTED_RELAY: &str = "wss://jskitty.cat/nostr";

static NOSTR_CLIENT: OnceCell<Client> = OnceCell::new();
static TAURI_APP: OnceCell<AppHandle> = OnceCell::new();

#[derive(serde::Serialize, Clone, Debug)]
struct Message {
    id: String,
    content: String,
    reactions: Vec<Reaction>,
    at: u64,
    pending: bool,
    failed: bool,
    mine: bool,
}

#[derive(serde::Serialize, Clone, Debug)]
struct Reaction {
    id: String,
    /** The HEX Event ID of the message being reacted to */
    reference_id: String,
    /** The HEX ID of the author */
    author_id: String,
    /** The emoji of the reaction */
    emoji: String,
}

#[derive(serde::Serialize, Clone, Debug)]
struct Profile {
    id: String,
    name: String,
    avatar: String,
    messages: Vec<Message>,
    status: Status,
    last_updated: u64,
    typing_until: u64,
    mine: bool,
}

impl Profile {
    fn new() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            avatar: String::new(),
            messages: Vec::new(),
            status: Status::new(),
            last_updated: 0,
            typing_until: 0,
            mine: false
        }
    }

    fn get_message_mut(&mut self, id: String) -> Result<&mut Message, ()> {
        if let Some(msg) = self.messages.iter_mut().find(|msg| msg.id == id) {
            return Ok(msg);
        } else {
            return Err(());
        }
    }

    fn from_metadata(&mut self, meta: Metadata) {
        self.name = meta.name.unwrap_or(self.name.clone());
        self.avatar = meta.picture.unwrap_or(self.avatar.clone());
    }

    fn internal_add_message(&mut self, message: Message) {
        // Make sure we don't add the same message twice
        if !self.messages.iter().any(|m| m.id == message.id) {
            // If it's their message; disable their typing indicator until further indicators are sent
            if !message.mine {
                self.typing_until = 0;
            }
            self.messages.push(message);
            // TODO: use appending/prepending and splicing, rather than sorting each message!
            // This is very expensive, but will do for now as a stop-gap.
            self.messages.sort_by(|a, b| a.at.cmp(&b.at));
        }
    }

    fn internal_add_reaction(&mut self, msg_id: String, reaction: Reaction) -> bool {
        // Find the message being reacted to
        match self.get_message_mut(msg_id) {
            Ok(msg) => {
                // Make sure we don't add the same reaction twice
                if !msg.reactions.iter().any(|r| r.id == reaction.id) {
                    msg.reactions.push(reaction);
                }
                true
            },
            Err(_) => false
        }
    }
}

#[derive(serde::Serialize, Clone, Debug)]
struct Status {
    title: String,
    purpose: String,
    url: String,
}

impl Status {
    fn new() -> Self {
        Self {
            title: String::new(),
            purpose: String::new(),
            url: String::new()
        }
    }
}

#[derive(serde::Serialize, Clone, Debug)]
struct ChatState {
    profiles: Vec<Profile>,
    // Used, particularly, for detecting Message + Profile changes and rendering them
    has_state_changed: bool,
}

impl ChatState {
    fn new() -> Self {
        Self {
            profiles: Vec::new(),
            has_state_changed: true,
        }
    }

    fn get_profile(&mut self, npub: String) -> Result<&Profile, ()> {
        if let Some(profile) = self.profiles.iter().find(|profile| profile.id == npub) {
            return Ok(profile);
        } else {
            return Err(());
        }
    }

    fn get_profile_mut(&mut self, npub: String) -> Result<&mut Profile, ()> {
        if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == npub) {
            return Ok(profile);
        } else {
            return Err(());
        }
    }

    fn add_message(&mut self, npub: String, message: Message) {
        if let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == npub) {
            // Add the message to the existing profile
            profile.internal_add_message(message);
        } else {
            // Generate the profile and add the message to it
            let mut profile = Profile::new();
            profile.id = npub;
            profile.internal_add_message(message);
            self.profiles.push(profile);
        }
        self.has_state_changed = true;
    }

    fn add_reaction(&mut self, npub: String, msg_id: String, reaction: Reaction) -> bool {
        // Get the profile
        match self.get_profile_mut(npub) {
            Ok(profile) => {
                // Add the reaction to the profile's message
                match profile.internal_add_reaction(msg_id, reaction) {
                    true => {
                        self.has_state_changed = true;
                        true
                    },
                    false => false
                }
            },
            Err(_) => false
        }
    }
}

lazy_static! {
    static ref STATE: Mutex<ChatState> = Mutex::new(ChatState::new());
}

#[tauri::command]
async fn fetch_messages(init: bool) -> Result<Vec<Profile>, ()> {
    // If we don't have any messages - keep trying to find 'em
    if init {
        let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

        // Grab our pubkey
        let signer = client.signer().await.unwrap();
        let my_public_key = signer.get_public_key().await.unwrap();

        // Fetch GiftWraps related to us
        let filter = Filter::new().pubkey(my_public_key).kind(Kind::GiftWrap);
        let events = client
            .fetch_events(vec![filter], std::time::Duration::from_secs(30))
            .await
            .unwrap();

        // Decrypt every GiftWrap and return their contents + senders
        for event in events.into_iter().filter(|e| e.kind == Kind::GiftWrap) {
            handle_event(event, false).await;
        }
    }

    Ok(STATE.lock().await.profiles.clone())
}

#[tauri::command]
async fn message(receiver: String, content: String) -> Result<bool, String> {
    // Immediately add the message to our state as "Pending", we'll update it as either Sent (non-pending) or Failed in the future
    let pending_count = STATE.lock().await.get_profile(receiver.clone()).unwrap_or(&Profile::new()).messages.iter().filter(|m| m.pending).count();
    let pending_id = String::from("pending-") + &pending_count.to_string();
    let msg = Message {
        id: pending_id.clone(),
        content: content.clone(),
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        reactions: Vec::new(),
        pending: true,
        failed: false,
        mine: true,
    };
    STATE.lock().await.add_message(receiver.clone(), msg);

    // Grab our pubkey
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");
    let signer = client.signer().await.unwrap();
    let my_public_key = signer.get_public_key().await.unwrap();

    // Convert the Bech32 String in to a PublicKey
    let receiver_pubkey = PublicKey::from_bech32(receiver.clone().as_str()).unwrap();

    // Build the NIP-17 rumor
    let rumor: UnsignedEvent = EventBuilder::private_msg_rumor(receiver_pubkey, content.clone()).build(my_public_key);

    // Send message to the real receiver
    match client.gift_wrap(&receiver_pubkey, rumor.clone(), []).await {
        Ok(_) => {
            // Send message to our own public key, to allow for message recovering
            match client.gift_wrap(&my_public_key, rumor.clone(), []).await {
                Ok(_) => {
                    // Mark the message as a success
                    let mut state = STATE.lock().await;
                    let chat = state.profiles.iter_mut().find(|chat| chat.id == receiver).unwrap();
                    let message = chat.get_message_mut(pending_id).unwrap();
                    message.id = rumor.id.unwrap().to_hex();
                    message.pending = false;
                    state.has_state_changed = true;
                    return Ok(true);
                }
                Err(_) => {
                    // This is an odd case; the message was sent to the receiver, but NOT ourselves
                    // We'll class it as sent, for now...
                    let mut state = STATE.lock().await;
                    let chat = state.profiles.iter_mut().find(|chat| chat.id == receiver).unwrap();
                    let message = chat.get_message_mut(pending_id).unwrap();
                    message.id = rumor.id.unwrap().to_hex();
                    message.pending = false;
                    state.has_state_changed = true;
                    return Ok(true)
                }
            }
        },
        Err(_) => {
            // Mark the message as a failure, bad message, bad!
            let mut state = STATE.lock().await;
            let chat = state.profiles.iter_mut().find(|chat| chat.id == receiver).unwrap();
            let failed_msg = chat.get_message_mut(pending_id).unwrap();
            failed_msg.failed = true;
            state.has_state_changed = true;
            return Ok(false);
        }
    }
}

#[tauri::command]
async fn react(reference_id: String, npub: String, emoji: String) -> Result<bool, ()> {
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

    // Prepare the EventID and Pubkeys for rumor building
    let reference_event = EventId::from_hex(reference_id.as_str()).unwrap();
    let receiver_pubkey = PublicKey::from_bech32(npub.as_str()).unwrap();

    // Grab our pubkey
    let signer = client.signer().await.unwrap();
    let my_public_key = signer.get_public_key().await.unwrap();

    // Build our NIP-25 Reaction rumor
    let rumor = EventBuilder::reaction_extended(
        reference_event,
        receiver_pubkey,
        Some(Kind::PrivateDirectMessage),
        emoji.clone(),
    );

    // Send reaction to the real receiver
    client
        .gift_wrap(&receiver_pubkey, rumor.clone(), [])
        .await
        .unwrap();

    // Send reaction to our own public key, to allow for recovering
    match client.gift_wrap(&my_public_key, rumor, []).await {
        Ok(response) => {
            // And add our reaction locally
            let reaction = Reaction {
                id: response.id().to_hex(),
                reference_id: reference_id.clone(),
                author_id: my_public_key.to_hex(),
                emoji,
            };
            return Ok(STATE.lock().await.add_reaction(npub, reference_id, reaction));
        }
        Err(e) => {
            eprintln!("Error: {:?}", e);
            return Ok(false);
        }
    }
}

#[tauri::command]
async fn load_profile(npub: String) -> Result<Profile, ()> {
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

    // Convert the Bech32 String in to a PublicKey
    let profile_pubkey = PublicKey::from_bech32(npub.as_str()).unwrap();

    // Grab our pubkey to check for profiles belonging to us
    let signer = client.signer().await.unwrap();
    let my_public_key = signer.get_public_key().await.unwrap();

    // Fetch an immutable profile from the cache (or, quickly generate a new one to pass to the fetching logic)
    // Mutex Scope: we want to hold this lock as short as possible, given this function is "spammed" for very fast profile cache hit checks
    let profile: Profile;
    {
        let mut state = STATE.lock().await;
        profile = match state.get_profile(npub.clone()) {
            Ok(p) => p,
            Err(_) => {
                // Create a new profile
                let mut new_profile = Profile::new();
                new_profile.id = npub.clone();
                state.profiles.push(new_profile);
                state.get_profile(npub.clone()).unwrap()
            }
        }.clone();

        // If the profile has been refreshed in the last 30s, return it's cached version
        if profile.last_updated + 30 > std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() {
            return Ok(profile.clone())
        }
    }

    // Attempt to fetch their status, if one exists
    let status_filter = Filter::new()
        .author(profile_pubkey)
        .kind(Kind::from_u16(30315))
        .limit(1);
    let status = match client
        .fetch_events(vec![status_filter], std::time::Duration::from_secs(10))
        .await
    {
        Ok(res) => {
            // Make sure they have a status available
            if !res.is_empty() {
                let status_event = res.first().unwrap();
                // Simple status recognition: last, general-only, no URLs, Metadata or Expiry considered
                // TODO: comply with expiries, accept more "d" types, allow URLs
                Status {
                    title: status_event.content.clone(),
                    purpose: status_event
                        .tags
                        .first()
                        .unwrap()
                        .content()
                        .unwrap()
                        .to_string(),
                    url: String::from(""),
                }
            } else {
                // No status
                Status::new()
            }
        }
        Err(_e) => Status::new(),
    };

    // Attempt to fetch their Metadata profile
    match client
        .fetch_metadata(profile_pubkey, std::time::Duration::from_secs(10))
        .await
    {
        Ok(meta) => {
            // If it's ours, mark it as such
            let mut state = STATE.lock().await;
            let profile_mutable = state.get_profile_mut(npub).unwrap();
            profile_mutable.mine = my_public_key == profile_pubkey;
            // Update the Status
            profile_mutable.status = status;
            // Update the Metadata
            profile_mutable.from_metadata(meta);
            // And apply the current update time
            profile_mutable.last_updated = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            // And mark the state as changed
            let ret_profile = profile_mutable.clone();
            state.has_state_changed = true;
            return Ok(ret_profile);
        }
        Err(_) => {
            return Ok(profile);
        }
    }
}

#[tauri::command]
async fn update_profile(name: String, avatar: String) -> Result<Profile, ()> {
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

    // Grab our pubkey
    let signer = client.signer().await.unwrap();
    let my_public_key = signer.get_public_key().await.unwrap();

    // Get our profile
    let mut meta: Metadata;
    let mut state = STATE.lock().await;
    let profile = state.get_profile(my_public_key.to_bech32().unwrap()).unwrap().clone();

    // We'll apply the changes to the previous profile and carry-on the rest
    meta = Metadata::new()
        .name(if name.is_empty() { profile.name.clone() } else { name });

    // Optional avatar
    if !avatar.is_empty() || !profile.avatar.is_empty() {
        meta = meta.picture(Url::parse(if avatar.is_empty() { profile.avatar.as_str() } else { avatar.as_str() }).unwrap());
    }

    // Broadcast the profile update
    match client.set_metadata(&meta).await {
        Ok(_event) => {
            // Apply our Metadata to our Profile
            let profile_mutable = state.get_profile_mut(my_public_key.to_bech32().unwrap()).unwrap();
            profile_mutable.from_metadata(meta);
            state.has_state_changed = true;
            Ok(profile.clone())
        },
        Err(_e) => { Err(()) }
    }
}

#[tauri::command]
async fn update_status(status: String) -> Result<Profile, ()> {
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

    // Grab our pubkey
    let signer = client.signer().await.unwrap();
    let my_public_key = signer.get_public_key().await.unwrap();

    // Build and broadcast the status
    let status_builder = EventBuilder::new(Kind::from_u16(30315), status.as_str()).tag(Tag::custom(TagKind::d(), vec!["general"]));
    match client.send_event_builder(status_builder).await {
        Ok(_event) => {
            // Add the status to our profile
            let mut state = STATE.lock().await;
            let profile = state.get_profile_mut(my_public_key.to_bech32().unwrap()).unwrap();
            profile.status.purpose = String::from("general");
            profile.status.title = status;
            Ok(profile.clone())
        },
        Err(_e) => { Err(()) },
    }
}

#[tauri::command]
async fn start_typing(receiver: String) -> Result<bool, ()> {
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

    // Convert our Bech32 receiver to a PublicKey
    let receiver_pubkey = PublicKey::from_bech32(receiver.as_str()).unwrap();

    // Grab our pubkey
    let signer = client.signer().await.unwrap();
    let my_public_key = signer.get_public_key().await.unwrap();

    // Build and broadcast the Typing Indicator
    let rumor = EventBuilder::new(Kind::ApplicationSpecificData, "typing")
        .tag(Tag::public_key(receiver_pubkey))
        .tag(Tag::custom(TagKind::d(), vec!["vector"]))
        .tag(Tag::expiration(Timestamp::from_secs(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 30)))
        .build(my_public_key);

    // Gift Wrap and send our Typing Indicator to receiver via our Trusted Relay
    // Note: we set a "public-facing" 1-hour expiry so that our trusted NIP-40 relay can purge old Typing Indicators
    let expiry_time = Timestamp::from_secs(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600);
    match client.gift_wrap_to([TRUSTED_RELAY], &receiver_pubkey, rumor.clone(), [Tag::expiration(expiry_time)]).await {
        Ok(_) => Ok(true),
        Err(_) => Ok(false)
    }
}



#[tauri::command]
async fn handle_event(event: Event, is_new: bool) {
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

    // Grab our pubkey
    let signer = client.signer().await.unwrap();
    let my_public_key = signer.get_public_key().await.unwrap();

    // Unwrap the gift wrap
    let mut state = STATE.lock().await;
    match client.unwrap_gift_wrap(&event).await {
        Ok(UnwrappedGift { rumor, sender }) => {
            // Check if it's mine
            let is_mine = sender == my_public_key;

            // Direct Message (NIP-17)
            if rumor.kind == Kind::PrivateDirectMessage {
                // Get contact public key (bech32)
                let contact: String = if is_mine {
                    // Try to get the first public key from tags
                    match rumor.tags.public_keys().next() {
                        Some(pub_key) => match pub_key.to_bech32() {
                            Ok(p_tag_pubkey_bech32) => p_tag_pubkey_bech32,
                            Err(_) => {
                                eprintln!("Failed to convert public key to bech32");
                                // If conversion fails, fall back to sender
                                sender.to_bech32().expect("Failed to convert sender's public key to bech32")
                            }
                        },
                        None => {
                            eprintln!("No public key tag found");
                            // If no public key found in tags, fall back to sender
                            sender.to_bech32().expect("Failed to convert sender's public key to bech32")
                        }
                    }
                } else {
                    // If not is_mine, just use sender's bech32
                    sender.to_bech32().expect("Failed to convert sender's public key to bech32")
                };

                // Send an OS notification for incoming messages
                if !is_mine && is_new {
                    // Find the name of the sender, if we have it
                    let profile = state.get_profile(contact.clone()).unwrap();
                    let display_name = match profile.name.is_empty() {
                        true => String::from("New Message"),
                        false => profile.name.clone()
                    };
                    show_notification(display_name, rumor.content.clone());
                }

                // Add to our state
                let msg = Message {
                    id: rumor.id.unwrap().to_hex(),
                    content: rumor.content,
                    at: rumor.created_at.as_u64(),
                    reactions: Vec::new(),
                    mine: is_mine,
                    pending: false,
                    failed: false
                };
                state.add_message(contact, msg);
            }
            // Emoji Reaction (NIP-25)
            else if rumor.kind == Kind::Reaction {
                match rumor.tags.find(TagKind::e()) {
                    Some(react_reference_tag) => {
                        // The message ID being 'reacted' to
                        let reference_id = react_reference_tag.content().unwrap();

                        // The contact (npub) sending us this reaction
                        let npub: String = if is_mine {
                            // Try to get the first public key from tags
                            match rumor.tags.public_keys().next() {
                                Some(pub_key) => match pub_key.to_bech32() {
                                    Ok(p_tag_pubkey_bech32) => p_tag_pubkey_bech32,
                                    Err(_) => {
                                        eprintln!("Failed to convert public key to bech32");
                                        // If conversion fails, fall back to sender
                                        sender.to_bech32().expect("Failed to convert sender's public key to bech32")
                                    }
                                },
                                None => {
                                    eprintln!("No public key tag found");
                                    // If no public key found in tags, fall back to sender
                                    sender.to_bech32().expect("Failed to convert sender's public key to bech32")
                                }
                            }
                        } else {
                            // If not is_mine, just use sender's bech32
                            sender.to_bech32().expect("Failed to convert sender's public key to bech32")
                        };

                        // Create the Reaction
                        let reaction = Reaction {
                            id: rumor.id.unwrap().to_hex(),
                            reference_id: reference_id.to_string(),
                            author_id: sender.to_hex(),
                            emoji: rumor.content.clone(),
                        };

                        // Add the reaction
                        match state.add_reaction(npub, reference_id.to_string(), reaction) {
                            true => {},
                            false => { println!("Couldn't find a profile for a reacted-to message, odd!") }
                        }
                    }
                    None => println!("No referenced message for reaction"),
                }
            }
            // Vector-specific events (NIP-78)
            else if rumor.kind == Kind::ApplicationSpecificData {
                // Ensure the application target is ours
                match rumor.tags.find(TagKind::d()) {
                    Some(d_tag) => {
                        if d_tag.content().unwrap() == "vector" {
                            // Typing Indicator
                            if rumor.content == "typing" {
                                // A NIP-40 expiry must be present
                                match rumor.tags.find(TagKind::Expiration) {
                                    Some(ex_tag) => {
                                        // And it must be within 30 seconds
                                        let expiry_timestamp: u64 = ex_tag.content().unwrap().parse().unwrap_or(0);
                                        // Check if the expiry timestamp is within 30 seconds from now (we'll say 35 to account for slight 'system time drift')
                                        let current_timestamp = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                                        if expiry_timestamp <= current_timestamp + 35 && expiry_timestamp > current_timestamp {
                                            // Now we apply the typing indicator to it's author profile
                                            match state.get_profile_mut(rumor.pubkey.to_bech32().unwrap()) {
                                                Ok(profile) => {
                                                    profile.typing_until = expiry_timestamp;
                                                    state.has_state_changed = true;
                                                },
                                                Err(_) => { /* Received a Typing Indicator from an unknown contact, ignoring... */ }
                                            };
                                        }
                                    },
                                    None => {}
                                }
                            }
                        }
                    },
                    None => {}
                }
            }
        }
        Err(_e) => (),
    }
}

#[tauri::command]
async fn notifs() -> Result<bool, String> {
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

    // Grab our pubkey
    let signer = client.signer().await.unwrap();
    let pubkey = signer.get_public_key().await.unwrap();

    // Listen for GiftWraps related to us
    let filter = Filter::new().pubkey(pubkey).kind(Kind::GiftWrap).limit(0);

    // Subscribe to the filter and begin handling incoming events
    match client.subscribe(vec![filter], None).await {
        Ok(_) => { /* Good! */ },
        Err(e) => { return Err(e.to_string()) }
    }
    match client
        .handle_notifications(|notification| async {
            if let RelayPoolNotification::Event { event, .. } = notification {
                handle_event(*event, true).await;
            }
            Ok(false)
        })
        .await {
            Ok(_) => Ok(true),
            Err(e) => { Err(e.to_string()) }
        }
}

#[tauri::command]
fn show_notification(title: String, content: String) {
    let app_handle = TAURI_APP.get().unwrap().clone();
    // Only send notifications if the app is not focused
    // TODO: generalise this assumption - it's only used for Message Notifications at the moment
    if !app_handle
        .webview_windows()
        .iter()
        .next()
        .unwrap()
        .1
        .is_focused()
        .unwrap()
    {
        app_handle
            .notification()
            .builder()
            .title(title)
            .body(content)
            .show()
            .unwrap_or_else(|e| eprintln!("Failed to send notification: {}", e));
    }
}

#[derive(serde::Serialize, Clone)]
struct LoginKeyPair {
    public: String,
    private: String,
}

#[tauri::command]
async fn login(import_key: String) -> Result<LoginKeyPair, ()> {
    let keys: Keys;
    // TODO: add validation, error handling, etc

    // If it's an nsec, import that
    if import_key.starts_with("nsec") {
        keys = Keys::parse(&import_key).unwrap();
    } else {
        // Otherwise, we'll try importing it as a mnemonic seed phrase (BIP-39)
        keys = Keys::from_mnemonic(import_key, Some(String::new())).unwrap();
    }

    // Initialise the Nostr client
    let client = Client::builder()
        .signer(keys.clone())
        .opts(Options::new().gossip(false))
        .build();
    NOSTR_CLIENT.set(client).unwrap();

    // Add our profile (at least, the npub of it) to our state
    let npub = keys.public_key.to_bech32().unwrap();
    let mut profile = Profile::new();
    profile.id = npub.clone();
    profile.mine = true;
    STATE.lock().await.profiles.push(profile);

    // Return our npub to the frontend client
    Ok( LoginKeyPair { public: npub, private: keys.secret_key().to_bech32().unwrap()} )
}

#[tauri::command]
async fn has_state_changed() -> Result<bool, ()> {
    Ok(STATE.lock().await.has_state_changed)
}

#[tauri::command]
async fn acknowledge_state_change() {
    STATE.lock().await.has_state_changed = false;
}

#[tauri::command]
async fn connect() {
    let client = NOSTR_CLIENT.get().expect("Nostr client not initialized");

    // Add our 'Trusted Relay' (see Rustdoc for TRUSTED_RELAY for more info)
    client.add_relay(TRUSTED_RELAY).await.unwrap();

    // Add a couple common relays, especially with explicit NIP-17 support (thanks 0xchat!)
    client.add_relay("wss://relay.0xchat.com").await.unwrap();
    client.add_relay("wss://auth.nostr1.com").await.unwrap();
    client.add_relay("wss://relay.damus.io").await.unwrap();

    // Connect!
    client.connect().await;
}

// Convert string to bytes, ensuring we're dealing with the raw content
fn string_to_bytes(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

// Convert bytes to string, but we'll use hex encoding for encrypted data
fn bytes_to_hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// Convert hex string back to bytes for decryption
fn hex_string_to_bytes(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i+2], 16).unwrap())
        .collect()
}

async fn hash_pass(password: String) -> [u8; 32] {
    // 75000 KiB memory size
    let memory = 75000;
    // 5 iterations
    let iterations = 5;
    let params = Params::new(memory, iterations, 1, Some(32)).unwrap();

    // TODO: create a random on-disk salt at first init
    // However, with the nature of this being local software, it won't help a user whom has their system compromised in the first place
    let salt = "vectorvectovectvecvev".as_bytes();

    // Prepare derivation
    let argon = Argon2::new(argon2::Algorithm::Argon2id, Version::V0x13, params);
    let mut key: [u8; 32] = [0; 32];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .unwrap();

    key
}

// Encrypt an Input with ChaCha20 and password-derived Argon2 key
// Output format: <12-byte-rand-nonce><encrypted-payload>
#[tauri::command]
async fn encrypt(input: String, password: String) -> String {
    // Hash our password with ramped-up Argon2 and use it as the key
    let key = hash_pass(password).await;

    // Generate a nonce
    let mut rng = rand::thread_rng();
    let nonce: [u8; 12] = rng.gen();

    // Prepend the nonce to our cipher output
    let mut buffer: Vec<u8> = nonce.to_vec();

    // Encrypt the input
    let mut cipher = ChaCha20::new(&key.into(), &nonce.into());
    let mut cipher_buffer = string_to_bytes(&input);
    cipher.apply_keystream(&mut cipher_buffer);

    // Append the cipher buffer
    buffer.append(&mut cipher_buffer);

    // Convert the encrypted bytes to a hex string for safe storage/transmission
    bytes_to_hex_string(&buffer)
}

#[tauri::command]
async fn decrypt(ciphertext: String, password: String) -> Result<String, ()> {
    // Hash our password with ramped-up Argon2 and use it as the key
    let key = hash_pass(password).await;

    // Prepare our decryption buffer split it away from our prepended nonce
    let mut nonce = hex_string_to_bytes(&ciphertext);
    let mut buffer = nonce.split_off(12);

    // Decrypt
    let mut cipher = ChaCha20::new(&key.into(), nonce.as_slice().into());
    cipher.apply_keystream(&mut buffer);

    // Convert decrypted bytes back to string
    match String::from_utf8(buffer) {
        Ok(decrypted) => Ok(decrypted),
        Err(_) => Err(())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .setup(|app| {
            let app_handle = app.app_handle().clone();
            // Set as our accessible static app handle
            TAURI_APP.set(app_handle).unwrap();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            fetch_messages,
            message,
            react,
            login,
            notifs,
            load_profile,
            update_profile,
            update_status,
            start_typing,
            connect,
            has_state_changed,
            acknowledge_state_change,
            encrypt,
            decrypt
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
