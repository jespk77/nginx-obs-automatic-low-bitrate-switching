use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use rust_i18n::t;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time;
use tracing::{debug, error, info};

use crate::chat::{self, HandleMessage, OptionalScene, Permission};
use crate::{config, error, events, switcher, user_manager, Noalbs};

pub struct ChatHandler {
    chat_handler_rx: mpsc::Receiver<super::HandleMessage>,
    user_manager: user_manager::UserManager,
    chat_senders: HashMap<chat::ChatPlatform, Arc<dyn chat::ChatLogic>>,

    timeouts: HashMap<chat::ChatPlatform, Vec<Timeout>>,
    default_commands: HashMap<chat::Command, config::CommandInfo>,
}

impl ChatHandler {
    pub fn new(
        chat_handler_rx: mpsc::Receiver<super::HandleMessage>,
        user_manager: user_manager::UserManager,
    ) -> Self {
        let mut timeouts = HashMap::new();
        timeouts.insert(chat::ChatPlatform::Twitch, Vec::new());

        Self {
            chat_handler_rx,
            user_manager,
            chat_senders: HashMap::new(),
            default_commands: Self::default_command_settings(),
            timeouts,
        }
    }

    // TODO: Should there also be default alias?
    fn default_command_settings() -> HashMap<chat::Command, config::CommandInfo> {
        let mut default = HashMap::new();

        use chat::Command;

        default.insert(
            Command::Switch,
            config::CommandInfo {
                ..Default::default()
            },
        );

        default.insert(
            Command::Bitrate,
            config::CommandInfo {
                permission: Some(Permission::Public),
                ..Default::default()
            },
        );

        default.insert(
            Command::Fix,
            config::CommandInfo {
                permission: Some(Permission::Mod),
                ..Default::default()
            },
        );

        default.insert(
            Command::ServerInfo,
            config::CommandInfo {
                permission: Some(Permission::Mod),
                ..Default::default()
            },
        );

        default.insert(
            Command::Otrigger,
            config::CommandInfo {
                permission: Some(Permission::Mod),
                ..Default::default()
            },
        );

        default.insert(
            Command::Refresh,
            config::CommandInfo {
                permission: Some(Permission::Mod),
                ..Default::default()
            },
        );

        default.insert(
            Command::Rtrigger,
            config::CommandInfo {
                permission: Some(Permission::Mod),
                ..Default::default()
            },
        );

        default.insert(
            Command::Sourceinfo,
            config::CommandInfo {
                permission: Some(Permission::Mod),
                ..Default::default()
            },
        );

        default.insert(
            Command::Trigger,
            config::CommandInfo {
                permission: Some(Permission::Mod),
                ..Default::default()
            },
        );

        default.insert(
            Command::Version,
            config::CommandInfo {
                permission: Some(Permission::Public),
                ..Default::default()
            },
        );

        default
    }

    pub fn add_chat_sender(
        &mut self,
        platform: chat::ChatPlatform,
        connection: Arc<dyn chat::ChatLogic>,
    ) {
        self.chat_senders.insert(platform, connection);
    }

    pub async fn handle_messages(&mut self) {
        while let Some(message) = self.chat_handler_rx.recv().await {
            debug!("Handle this message: {:?}", message);

            match message {
                HandleMessage::ChatMessage(msg) => {
                    if msg.message.is_empty() {
                        continue;
                    }

                    self.handle_chat_message(msg).await;
                }
                HandleMessage::InternalChatUpdate(update) => {
                    use chat::InternalUpdate;
                    match update.kind {
                        InternalUpdate::Raided(ref target_info) => {
                            let target_info = target_info.to_owned();
                            self.handle_raid(update, target_info).await
                        }
                        InternalUpdate::OfflineTimeout => self.handle_offline_timeout(update).await,
                    };
                }
                HandleMessage::AutomaticSwitchingScene(ss) => {
                    self.handle_automatic_switching_message(ss).await;
                }
            }
        }
    }

    pub async fn handle_automatic_switching_message(
        &self,
        ss: chat::AutomaticSwitchingScene,
    ) -> Option<()> {
        let sender = self.chat_senders.get(&ss.platform)?;

        let user = self
            .user_manager
            .get_user_by_chat_platform(&ss.channel, &ss.platform)
            .await?;
        let lang = &user.chat_language().await.unwrap().to_string();
        let mut msg = t!("sceneSwitch.switch", locale = lang, scene = &ss.scene);

        use switcher::SwitchType::*;
        match ss.switch_type {
            Normal | Low => {
                let bitrate = bitrate_msg(&user, lang).await;
                let _ = write!(msg, " | {}", bitrate);
            }
            Previous | Offline => {}
        }

        sender.send_message(ss.channel, msg).await;

        Some(())
    }

    pub async fn handle_chat_message(&mut self, msg: chat::ChatMessage) -> Option<()> {
        let user = self
            .user_manager
            .get_user_by_chat_platform(&msg.channel, &msg.platform)
            .await?;

        let (command, permission) = self.get_command(&user, &msg).await?;

        if !self
            .is_allowed_to_use_command(&user, &msg, &permission)
            .await?
        {
            debug!("{} not allowed to use command {:?}", msg.sender, command);
            return None;
        }

        if msg.permission == chat::Permission::Public
            && self.handle_timeout(&msg.platform, &msg.channel).await
        {
            debug!("Timeout");
            return None;
        }

        match command {
            chat::Command::Unknown(_) => {}
            _ => {
                info!(
                    "[{}] {} executed command: {:?}",
                    msg.channel, msg.sender, command
                );
            }
        }

        let dc = DispatchCommand {
            user: user.clone(),
            lang: user.chat_language().await.unwrap().to_string(),
            chat_sender: self.chat_senders.get(&msg.platform)?.clone(),
            command,
            chat_message: msg,
        };

        tokio::spawn(async move { dc.run_command().await });

        Some(())
    }

    // TODO: refactor this
    /// Returns the command
    async fn get_command(
        &self,
        user: &Noalbs,
        msg: &chat::ChatMessage,
    ) -> Option<(chat::Command, chat::CommandPermissions)> {
        let state = user.state.read().await;
        let chat = state.config.chat.as_ref()?;
        let prefix = &chat.prefix;

        let mut message = msg.message.split_ascii_whitespace();
        let command = message.next().unwrap().strip_prefix(prefix)?;
        let mut command = super::Command::from(command);

        if let super::Command::Unknown(ref cmd) = command {
            if let Some(cmd_from_alias) =
                try_get_command_from_alias(&chat.commands, &self.default_commands, cmd)
            {
                command = cmd_from_alias;
            }
        }

        let permission = get_permission(&command, &chat.commands, &self.default_commands);

        debug!(
            "Found command: {:?}, with permission: {:?}",
            command, permission
        );

        Some((command, permission))
    }

    async fn is_allowed_to_use_command(
        &self,
        user: &Noalbs,
        msg: &chat::ChatMessage,
        permission: &chat::CommandPermissions,
    ) -> Option<bool> {
        let state = user.state.read().await;
        let chat = state.config.chat.as_ref()?;

        let chat::CommandPermissions {
            permission,
            user_permissions,
        } = permission;

        let user_permission = &msg.permission;

        if *user_permission == chat::Permission::Admin || chat.admins.contains(&msg.sender) {
            return Some(true);
        }

        if let Some(user_permissions) = user_permissions {
            if user_permissions.contains(&msg.sender) {
                return Some(true);
            }
        }

        if *user_permission == chat::Permission::Mod
            && !chat.enable_public_commands
            && !chat.enable_mod_commands
        {
            debug!("Public and Mod commands disabled");
            return Some(false);
        }

        if let Some(permission) = permission {
            if *user_permission == chat::Permission::Mod
                && *permission == chat::Permission::Mod
                && !chat.enable_mod_commands
            {
                debug!("Mod commands disabled");
                return Some(false);
            }
        }

        if *user_permission == chat::Permission::Public && !chat.enable_public_commands {
            debug!("Public commands disabled");
            return Some(false);
        }

        debug!("Not an admin checking permission");
        if let Some(permission) = permission {
            return Some(permission_is_allowed(permission, user_permission));
        }

        Some(false)
    }

    pub async fn handle_raid(
        &self,
        raid: chat::InternalChatUpdate,
        target_info: chat::RaidedInfo,
    ) -> Option<()> {
        let user = self
            .user_manager
            .get_user_by_chat_platform(&raid.channel, &raid.platform)
            .await?;

        let state = user.state.read().await;

        if !state
            .config
            .chat
            .as_ref()?
            .enable_auto_stop_stream_on_host_or_raid
        {
            return None;
        }

        let bs = &state.broadcasting_software;
        if !bs.is_streaming || bs.last_stream_started_at.elapsed().as_secs() < 60 {
            return None;
        }

        let command = if state.config.chat.as_ref()?.announce_raid_on_auto_stop {
            chat::Command::StopOnRaid(target_info)
        } else {
            chat::Command::Stop
        };

        info!(
            "Channel raided, stopping the stream ({:?}) {}",
            raid.platform, raid.channel
        );

        let dc = DispatchCommand {
            user: user.clone(),
            lang: user.chat_language().await.unwrap().to_string(),
            chat_sender: self.chat_senders.get(&raid.platform)?.clone(),
            command,
            chat_message: chat::ChatMessage {
                platform: raid.platform,
                permission: chat::Permission::Admin,
                channel: raid.channel,
                sender: "NOALBSbot".to_string(),
                message: "Hi your channel raided and I would like to stop it for you".to_string(),
            },
        };

        tokio::spawn(async move { dc.run_command().await });

        Some(())
    }

    pub async fn handle_offline_timeout(&self, host: chat::InternalChatUpdate) -> Option<()> {
        let sender = self.chat_senders.get(&host.platform)?;
        let user = self
            .user_manager
            .get_user_by_chat_platform(&host.channel, &host.platform)
            .await?;
        let lang = &user.chat_language().await.unwrap().to_string();
        let msg = t!("offlineTimeout.timeout", locale = lang);

        sender.send_message(host.channel, msg).await;

        Some(())
    }

    // TODO: Maybe remove when timeout passed
    pub async fn handle_timeout(&mut self, platform: &chat::ChatPlatform, channel: &str) -> bool {
        let platform_timeouts = self.timeouts.get_mut(platform).unwrap();
        let channel_timeout = platform_timeouts.iter_mut().find(|x| x.channel == channel);

        if let Some(timeout) = channel_timeout {
            let delta = timeout.time.elapsed().as_secs();

            if delta <= 5 {
                return true;
            } else {
                timeout.time = std::time::Instant::now();
            }
        } else {
            platform_timeouts.push(Timeout {
                channel: channel.to_owned(),
                time: std::time::Instant::now(),
            });
        }

        false
    }
}

fn get_permission(
    command: &chat::Command,
    user_commands: &Option<HashMap<chat::Command, config::CommandInfo>>,
    default_commands: &HashMap<chat::Command, config::CommandInfo>,
) -> chat::CommandPermissions {
    user_commands
        .as_ref()
        .and_then(|uc| try_get_permission(command, uc))
        .map(|mut p| {
            if p.permission.is_none() {
                p.permission =
                    try_get_permission(command, default_commands).and_then(|d| d.permission);
            }
            p
        })
        .or_else(|| try_get_permission(command, default_commands))
        .unwrap_or(chat::CommandPermissions {
            permission: Some(chat::Permission::Admin),
            user_permissions: None,
        })
}

fn try_get_permission(
    command: &chat::Command,
    commands: &HashMap<chat::Command, config::CommandInfo>,
) -> Option<chat::CommandPermissions> {
    if let Some(command) = commands.get(command) {
        return Some(chat::CommandPermissions {
            permission: command.permission.to_owned(),
            user_permissions: command.user_permissions.to_owned(),
        });
    }

    None
}

fn permission_is_allowed(
    permission: &chat::Permission,
    user_permission: &chat::Permission,
) -> bool {
    permission == &chat::Permission::Public
        || (permission == &chat::Permission::Mod && user_permission == &chat::Permission::Mod)
}

fn try_get_command_from_alias(
    user_commands: &Option<HashMap<chat::Command, config::CommandInfo>>,
    default_commands: &HashMap<chat::Command, config::CommandInfo>,
    potential_command: &str,
) -> Option<chat::Command> {
    // check if user defined alias
    if let Some(user_cmd) = user_commands {
        if let Some(cmd) = get_command_from_alias_string(user_cmd, potential_command) {
            return Some(cmd);
        }
    }

    if let Some(cmd) = get_command_from_alias_string(default_commands, potential_command) {
        return Some(cmd);
    }

    // TODO: check if platform specific?

    None
}

// TODO: This could be better
pub fn get_command_from_alias_string(
    commands: &HashMap<chat::Command, config::CommandInfo>,
    alias: &str,
) -> Option<chat::Command> {
    commands.iter().find_map(|(key, value)| {
        if let Some(aliases) = &value.alias {
            if aliases.iter().any(|x| x == alias) {
                return Some(key.to_owned());
            }
        }

        None
    })
}

pub struct DispatchCommand {
    user: Arc<Noalbs>,
    lang: String,
    chat_sender: Arc<dyn chat::ChatLogic>,
    command: chat::Command,
    chat_message: chat::ChatMessage,
}

impl DispatchCommand {
    pub async fn run_command(&self) {
        let mut params = self.chat_message.message.split_whitespace();
        params.next();

        match &self.command {
            chat::Command::Alias => self.alias(params).await,
            chat::Command::Autostop => self.autostop(params.next()).await,
            chat::Command::Bitrate => self.bitrate().await,
            chat::Command::Fix => self.fix().await,
            chat::Command::Refresh => self.refresh().await,
            chat::Command::Noalbs => self.noalbs(params.next(), params).await,
            chat::Command::Notify => self.notify(params.next()).await,
            chat::Command::Rec => self.record().await,
            chat::Command::Start => self.start().await,
            chat::Command::Stop => self.stop(None).await,
            chat::Command::Switch => self.switch(params.next()).await,
            chat::Command::Mute => self.mute(params.next()).await,
            chat::Command::Trigger => {
                self.trigger(switcher::TriggerType::Low, params.next())
                    .await
            }
            chat::Command::Otrigger => {
                self.trigger(switcher::TriggerType::Offline, params.next())
                    .await
            }
            chat::Command::Ortrigger => {
                self.trigger(switcher::TriggerType::RttOffline, params.next())
                    .await
            }
            chat::Command::Rtrigger => {
                self.trigger(switcher::TriggerType::Rtt, params.next())
                    .await
            }
            chat::Command::Version => self.version().await,
            chat::Command::PrivacyScene => {
                self.switch_optional_scene(chat::OptionalScene::Privacy)
                    .await
            }
            chat::Command::StartingScene => {
                self.switch_optional_scene(chat::OptionalScene::Starting)
                    .await
            }
            chat::Command::EndingScene => {
                self.switch_optional_scene(chat::OptionalScene::Ending)
                    .await
            }
            chat::Command::LiveScene => self.live_scene().await,
            chat::Command::ServerInfo => self.server_info().await,
            chat::Command::Mod => self.enable_mod(params.next()).await,
            chat::Command::Public => self.enable_public(params.next()).await,
            chat::Command::Sourceinfo => self.source_info(params.next()).await,
            chat::Command::Source => self.source(params.next()).await,
            chat::Command::Unknown(_) => {}

            chat::Command::StopOnRaid(target_info) => {
                self.stop_on_raid(target_info.to_owned()).await;
            }
            chat::Command::Collection => self.collection(params.next()).await,
        };
    }

    async fn alias<'a, I>(&self, args: I)
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut args = args.into_iter();
        let a1 = args.next();
        let a2 = args.next();

        if a1.is_none() || a2.is_none() {
            self.send(t!("alias.errorIncorrectArguments", locale = &self.lang))
                .await;
            return;
        }

        let a1 = a1.unwrap();
        let a2 = a2.unwrap();

        // remove alias
        if a1 == "rem" {
            if !&self.user.contains_alias(a2).await.unwrap() {
                self.send(t!("alias.errorAlias", locale = &self.lang, alias = a2))
                    .await;
                return;
            }

            if let Ok(success) = self.user.remove_alias(a2).await {
                if success {
                    self.save_config().await;
                    self.send(t!("alias.removed", locale = &self.lang, alias = a2))
                        .await;
                }
            }

            return;
        }

        // add alias
        if self.user.contains_alias(a1).await.unwrap() {
            self.send(t!(
                "alias.errorAlreadyUsed",
                locale = &self.lang,
                alias = a1
            ))
            .await;
            return;
        }

        let command = super::Command::from(a2);

        if let chat::Command::Unknown(_) = command {
            self.send(t!("alias.errorCommand", locale = &self.lang, command = a2))
                .await;
            return;
        }

        if self.user.add_alias(a1.to_string(), command).await.is_ok() {
            self.save_config().await;
            self.send(t!(
                "alias.success",
                locale = &self.lang,
                alias = a1,
                command = a2
            ))
            .await;
        }
    }

    async fn bitrate(&self) {
        let msg = bitrate_msg(&self.user, &self.lang).await;

        self.send(msg).await;
    }

    // TODO: more than one word?
    async fn switch(&self, name: Option<&str>) {
        let name = match name {
            Some(name) => name,
            None => {
                self.send(t!("switch.noParams", locale = &self.lang)).await;
                return;
            }
        };

        let msg = match self.switch_scene(name).await {
            Ok(scene) => t!("switch.success", locale = &self.lang, scene = &scene),
            Err(e) => {
                error!("{}", e);
                t!("switch.error", locale = &self.lang, scene = name)
            }
        };

        self.send(msg).await;
    }

    async fn mute(&self, scene: Option<&str>) {
        let scene = scene.unwrap_or_else(|| "belabox");

        let msg = match self.toggle_mute(scene).await {
            Ok(result) => {
                let status = if result.1 { "muted" } else { "unmuted" };
                t!("mute.success", locale = &self.lang, scene = &result.0, muted = &status)
            },
            Err(e) => {
                error!("{}", e);
                t!("mute.error", locale = &self.lang, scene = scene)
            }
        };

        self.send(msg).await;
    }

    async fn start(&self) {
        let (twitch_transcoding, record, starting) = {
            let state = self.user.state.read().await;
            let options = &state.config.optional_options;
            (
                options.twitch_transcoding_check,
                options.record_while_streaming,
                options.switch_to_starting_scene_on_stream_start,
            )
        };

        let success =
            if self.chat_message.platform == chat::ChatPlatform::Twitch && twitch_transcoding {
                self.start_twitch_transcoding().await
            } else {
                self.start_normal().await
            };

        if success && starting {
            self.switch_optional_scene(OptionalScene::Starting).await;
        }

        if success && record {
            self.record().await;
        }
    }

    async fn start_bsc(&self) -> Result<(), error::Error> {
        let state = self.user.state.read().await;

        let bsc = state
            .broadcasting_software
            .connection
            .as_ref()
            .ok_or(error::Error::UnableInitialConnection)?;

        bsc.start_streaming().await
    }

    async fn stop_bsc(&self) -> Result<(), error::Error> {
        let state = self.user.state.read().await;

        let bsc = state
            .broadcasting_software
            .connection
            .as_ref()
            .ok_or(error::Error::UnableInitialConnection)?;

        bsc.stop_streaming().await
    }

    async fn start_normal(&self) -> bool {
        let start = self.start_bsc().await;

        let msg = match start {
            Ok(_) => t!("start.success", locale = &self.lang),
            Err(ref e) => t!("start.error", locale = &self.lang, error = &e.to_string()),
        };

        self.send(msg).await;

        start.is_ok()
    }

    async fn start_twitch_transcoding(&self) -> bool {
        let (retry, delay) = {
            let options = &self.user.state.read().await.config.optional_options;
            let retry = options.twitch_transcoding_retries;
            let delay = options.twitch_transcoding_delay_seconds;

            (retry, delay)
        };

        self.send(t!("startTwitchTranscoding.trying", locale = &self.lang))
            .await;

        let mut attempts = 0;

        for i in 0..retry {
            debug!("[{}] Starting stream", i);
            if let Err(e) = self.start_bsc().await {
                self.send(t!(
                    "start.error",
                    locale = &self.lang,
                    error = &e.to_string()
                ))
                .await;
                return false;
            };

            time::sleep(time::Duration::from_secs(delay)).await;

            if let Ok(true) = check_if_transcoding(&self.chat_message.channel).await {
                attempts = i + 1;
                break;
            }

            if i == retry - 1 {
                debug!("[{}] Can't get transcoding", i);
                self.send(t!(
                    "startTwitchTranscoding.successNoTranscoding",
                    locale = &self.lang
                ))
                .await;
                return true;
            }

            debug!("[{}] Stopping stream", i);
            if let Err(e) = self.stop_bsc().await {
                self.send(t!(
                    "stop.error",
                    locale = &self.lang,
                    error = &e.to_string()
                ))
                .await;
                return false;
            };

            time::sleep(time::Duration::from_secs(5)).await;
        }

        let mut att_msg = String::new();

        if attempts > 1 {
            att_msg = t!(
                "startTwitchTranscoding.attempts",
                locale = &self.lang,
                count = &attempts.to_string()
            );
        }

        let msg = t!(
            "startTwitchTranscoding.success",
            locale = &self.lang,
            attemptsMessage = &att_msg
        );

        self.send(msg).await;

        true
    }

    async fn stop(&self, raid: Option<chat::RaidedInfo>) {
        let record = {
            self.user
                .state
                .read()
                .await
                .config
                .optional_options
                .record_while_streaming
        };

        let stop = self.stop_bsc().await;

        let success_msg = if let Some(info) = raid {
            t!(
                "stop.raid",
                locale = &self.lang,
                channel = &info.target,
                display_channel = &info.display
            )
        } else {
            t!("stop.success", locale = &self.lang)
        };

        let msg = match stop {
            Ok(_) => success_msg,
            Err(ref e) => t!("stop.error", locale = &self.lang, error = &e.to_string()),
        };

        self.send(msg).await;

        if stop.is_ok() && record {
            self.record().await;
        }
    }

    async fn trigger(&self, kind: switcher::TriggerType, value_string: Option<&str>) {
        let symbol = match kind {
            switcher::TriggerType::Low | switcher::TriggerType::Offline => "Kbps",
            switcher::TriggerType::Rtt | switcher::TriggerType::RttOffline => "ms",
        };

        let value = match value_string {
            Some(name) => name,
            None => {
                let msg = match &self.user.get_trigger_by_type(kind).await {
                    Some(bitrate) => t!(
                        "trigger.current",
                        locale = &self.lang,
                        number = &format!("{} {}", bitrate, symbol)
                    ),
                    None => t!("trigger.disabled", locale = &self.lang),
                };

                self.send(msg).await;
                return;
            }
        };

        let value = match value.parse::<u32>() {
            Ok(v) => v,
            Err(_) => {
                let msg = t!("trigger.error", locale = &self.lang, number = value);
                self.send(msg).await;
                return;
            }
        };

        let msg = match &self.user.update_trigger(kind, value).await {
            Some(value) => t!(
                "trigger.success",
                locale = &self.lang,
                number = &format!("{} {}", value, symbol)
            ),
            None => t!(
                "trigger.successDisabled",
                locale = &self.lang,
                number = &value.to_string()
            ),
        };

        self.save_config().await;
        self.send(msg).await;
    }

    async fn notify(&self, enabled: Option<&str>) {
        if let Some(enabled) = enabled {
            if let Ok(b) = enabled_to_bool(enabled) {
                self.user.set_notify(b).await;
                self.save_config().await;
            }
        }

        let msg = t!(
            "handleCommands.notify",
            locale = &self.lang,
            condition = &condition_to_text(self.user.get_notify().await, &self.lang)
        );

        self.send(msg).await;
    }

    async fn autostop(&self, enabled: Option<&str>) {
        if let Some(enabled) = enabled {
            if let Ok(b) = enabled_to_bool(enabled) {
                self.user.set_autostop(b).await.unwrap();
                self.save_config().await;
            }
        }

        let msg = t!(
            "handleCommands.autostop",
            locale = &self.lang,
            condition = &condition_to_text(self.user.get_autostop().await.unwrap(), &self.lang)
        );

        self.send(msg).await;
    }

    async fn fix(&self) {
        let state = self.user.state.read().await;

        let bsc = match &state.broadcasting_software.connection {
            Some(b) => b,
            None => return,
        };

        self.send(t!("fix.try", locale = &self.lang)).await;

        if bsc.fix().await.is_err() {
            let msg = t!("fix.error", locale = &self.lang);
            self.send(msg).await;
        };
    }

    async fn refresh(&self) {
        let state = self.user.state.read().await;

        let scene = match &state.config.optional_scenes.refresh {
            Some(s) => s.to_owned(),
            None => {
                self.send(t!("refresh.noScene", locale = &self.lang)).await;
                self.fix().await;
                return;
            }
        };
        let prev_scene = state.broadcasting_software.current_scene.to_owned();
        drop(state);

        self.send(t!("refresh.try", locale = &self.lang)).await;

        let err = t!("refresh.error", locale = &self.lang);
        if (self.switch_scene(&scene).await).is_err() {
            self.send(err).await;
            return;
        };

        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        if (self.switch_scene(&prev_scene).await).is_err() {
            self.send(err).await;
            return;
        };

        self.send(t!("refresh.success", locale = &self.lang)).await;
    }

    async fn switch_scene(&self, scene: &str) -> Result<String, error::Error> {
        self.user
            .state
            .read()
            .await
            .broadcasting_software
            .connection
            .as_ref()
            .ok_or(error::Error::NoSoftwareSet)?
            .switch_scene(scene)
            .await
    }

    async fn toggle_mute(&self, scene: &str) -> Result<(String, bool), error::Error> {
        self.user
            .state
            .read()
            .await
            .broadcasting_software
            .connection
            .as_ref()
            .ok_or(error::Error::NoSoftwareSet)?
            .toggle_mute(scene)
            .await
    }

    // Record is a toggle
    async fn record(&self) {
        let state = self.user.state.read().await;

        let bsc = match &state.broadcasting_software.connection {
            Some(b) => b,
            None => return,
        };

        let is_recording = match bsc.is_recording().await {
            Ok(status) => status,
            Err(_) => {
                self.send(t!("rec.errorStatus", locale = &self.lang)).await;
                return;
            }
        };

        if bsc.toggle_recording().await.is_err() {
            self.send(t!("rec.errorToggle", locale = &self.lang)).await;
            return;
        }

        if is_recording {
            self.send(t!("rec.stopped", locale = &self.lang)).await;
            return;
        }

        self.send(t!("rec.started", locale = &self.lang)).await;
    }

    pub async fn version(&self) {
        let msg = format!("Running NOALBS v{}", crate::VERSION);
        self.send(msg).await;
    }

    pub async fn noalbs<'a, I>(&self, command: Option<&str>, args: I)
    where
        I: IntoIterator<Item = &'a str>,
    {
        let command = match command {
            Some(command) => command,
            None => return,
        };

        let mut args = args.into_iter();

        let msg = match command {
            "version" => {
                self.version().await;
                return;
            }
            "prefix" => {
                if let Some(prefix) = args.next() {
                    let _ = self.user.set_prefix(prefix.to_owned()).await;
                    self.save_config().await;

                    self.user
                        .send_event(events::Event::PrefixChanged { prefix })
                        .await;

                    format!("NOALBS prefix updated to {}", prefix)
                } else {
                    "Can't update NOALBS prefix".to_string()
                }
            }
            "start" => {
                self.user.set_bitrate_switcher_state(true).await;
                self.save_config().await;
                t!("noalbs.switcherEnabled", locale = &self.lang)
            }
            "stop" => {
                self.user.set_bitrate_switcher_state(false).await;
                self.save_config().await;
                t!("noalbs.switcherDisabled", locale = &self.lang)
            }
            "instant" => {
                let toggle = self.user.set_instantly_switch_on_recover().await;
                self.save_config().await;
                t!(
                    "noalbs.instantSwitch",
                    locale = &self.lang,
                    condition = &condition_to_text(toggle, &self.lang)
                )
            }
            "lang" => {
                if let Some(lang) = args.next() {
                    if let Ok(l) = lang.parse::<super::ChatLanguage>() {
                        self.user.set_chat_language(l).await.unwrap();
                        self.save_config().await;
                        t!(
                            "noalbs.langSuccess",
                            locale = &lang.to_lowercase(),
                            lang = lang
                        )
                    } else {
                        t!("noalbs.langErrorInvalid", locale = &self.lang, lang = lang)
                    }
                } else {
                    t!("noalbs.langError", locale = &self.lang)
                }
            }
            "retry" => self.set_retry_attempts(args.next()).await,
            _ => String::new(),
        };

        if !msg.is_empty() {
            self.send(msg).await;
        }
    }

    async fn set_retry_attempts(&self, value_string: Option<&str>) -> String {
        let value = match value_string {
            Some(name) => name,
            None => {
                let current_attempts = &self.user.get_retry_attempts().await;

                return t!(
                    "noalbs.retryCount",
                    locale = &self.lang,
                    count = &current_attempts.to_string()
                );
            }
        };

        let value = match value.parse::<u8>() {
            Ok(v) => v,
            Err(_) => {
                return t!("noalbs.retryError", locale = &self.lang, count = value);
            }
        };

        self.user.set_retry_attempts(value).await;
        self.save_config().await;

        t!(
            "noalbs.retrySuccess",
            locale = &self.lang,
            count = &value.to_string()
        )
    }

    async fn switch_optional_scene(&self, scene_name: chat::OptionalScene) {
        let state = self.user.state.read().await;
        let optional_scenes = &state.config.optional_scenes;
        let scene = match scene_name {
            OptionalScene::Privacy => &optional_scenes.privacy,
            OptionalScene::Starting => &optional_scenes.starting,
            OptionalScene::Ending => &optional_scenes.ending,
        };

        if let Some(scene) = scene {
            self.send(t!("scene.success", locale = &self.lang, scene = scene_name))
                .await;
            self.switch(Some(scene)).await;
        } else {
            self.send(t!("scene.error", locale = &self.lang, scene = scene_name))
                .await;
        }
    }

    // TODO: Actually switch to the right scene
    async fn live_scene(&self) {
        let state = self.user.state.read().await;
        let scene = &state.config.switcher.switching_scenes.normal;

        self.send(t!("scene.success", locale = &self.lang, scene = "live"))
            .await;
        self.switch(Some(scene)).await;
    }

    async fn source_info(&self, server_name: Option<&str>) {
        let state = &self.user.state.read().await;
        let stream_servers = &state.config.switcher.stream_servers;

        let no_info = t!("sourceinfo.noInfo", locale = &self.lang);

        if let Some(name) = server_name {
            let server = match stream_servers.iter().find(|s| s.name == name) {
                Some(s) => s,
                None => {
                    let msg = t!("sourceinfo.noInfo", locale = &self.lang, name = name);
                    self.send(msg).await;

                    return;
                }
            };

            let info = match server.stream_server.source_info().await {
                Some(i) => i,
                None => no_info,
            };
            self.send(format!("{}: {}", name, info)).await;

            return;
        }

        let mut msg = Vec::new();

        for s in stream_servers {
            let info = s.stream_server.source_info().await;

            if let Some(info) = info {
                msg.push(format!("{}: {}", s.name, info));
            }
        }

        if msg.is_empty() {
            self.send(no_info).await;

            return;
        }

        self.send(msg.join(" - ")).await;
    }

    async fn enable_mod(&self, enabled: Option<&str>) {
        if let Some(enabled) = enabled {
            if let Ok(b) = enabled_to_bool(enabled) {
                self.user.set_enable_mod(b).await.unwrap();
                self.save_config().await;
            }
        }

        let msg = t!(
            "handleCommands.mod",
            locale = &self.lang,
            condition = &condition_to_text(self.user.get_enable_mod().await.unwrap(), &self.lang)
        );

        self.send(msg).await;
    }

    async fn enable_public(&self, enabled: Option<&str>) {
        if let Some(enabled) = enabled {
            if let Ok(b) = enabled_to_bool(enabled) {
                self.user.set_enable_public(b).await.unwrap();
                self.save_config().await;
            }
        }

        let msg = t!(
            "handleCommands.public",
            locale = &self.lang,
            condition =
                &condition_to_text(self.user.get_enable_public().await.unwrap(), &self.lang)
        );

        self.send(msg).await;
    }

    async fn send(&self, message: String) {
        self.chat_sender
            .send_message(self.chat_message.channel.to_owned(), message)
            .await;
    }

    async fn save_config(&self) {
        if let Err(e) = self.user.save_config().await {
            error!("Error saving config: {}", e)
        }
    }

    async fn server_info(&self) {
        let state = self.user.state.read().await;

        if state.broadcasting_software.initial_stream_status.is_none() {
            self.send(t!("serverinfo.noInfo", locale = &self.lang))
                .await;
            return;
        };

        let bsc = match &state.broadcasting_software.connection {
            Some(b) => b,
            None => return,
        };

        let ss = match bsc.info(&state).await {
            Ok(ss) => ss,
            Err(_) => {
                self.send(t!("serverinfo.noInfo", locale = &self.lang))
                    .await;
                return;
            }
        };

        let network = format!(
            "{} ({:.1}%)",
            ss.num_dropped_frames,
            (ss.num_dropped_frames as f64 / ss.num_total_frames as f64) * 100.0,
        );

        let rendering = format!(
            "{} ({:.1}%)",
            ss.render_missed_frames,
            (ss.render_missed_frames as f64 / ss.render_total_frames as f64) * 100.0,
        );

        let encoding = format!(
            "{} ({:.1}%)",
            ss.output_skipped_frames,
            (ss.output_skipped_frames as f64 / ss.output_total_frames as f64) * 100.0,
        );

        let scene = &state.broadcasting_software.current_scene;

        let msg = t!(
            "serverinfo.success",
            locale = &self.lang,
            fps = &ss.fps.round().to_string(),
            bitrate = &ss.bitrate.to_string(),
            network = &network,
            rendering = &rendering,
            encoding = &encoding,
            scene = scene
        );

        self.send(msg).await;
    }

    async fn source(&self, name: Option<&str>) {
        let name = match name {
            Some(name) => name,
            None => {
                self.send(t!("source.noParams", locale = &self.lang)).await;
                return;
            }
        };

        let msg = match self.toggle_source(name).await {
            Ok((scene, status)) => {
                let status = if status {
                    t!("source.enabled", locale = &self.lang)
                } else {
                    t!("source.disabled", locale = &self.lang)
                };

                t!(
                    "source.success",
                    locale = &self.lang,
                    name = &scene,
                    status = &status
                )
            }
            Err(e) => {
                error!("{}", e);
                t!("source.error", locale = &self.lang, name = name)
            }
        };

        self.send(msg).await;
    }

    async fn toggle_source(&self, scene: &str) -> Result<(String, bool), error::Error> {
        self.user
            .state
            .read()
            .await
            .broadcasting_software
            .connection
            .as_ref()
            .ok_or(error::Error::NoSoftwareSet)?
            .toggle_source(scene)
            .await
    }

    async fn stop_on_raid(&self, target_info: chat::RaidedInfo) {
        self.stop(Some(target_info)).await;
    }

    async fn collection(&self, name: Option<&str>) {
        let name = match name {
            Some(name) => name.to_lowercase(),
            None => {
                self.send(t!("collection.noParams", locale = &self.lang,))
                    .await;
                return;
            }
        };

        let state = self.user.state.read().await;
        let collections = match &state.config.software {
            config::SoftwareConnection::ObsOld(o) => &o.collections,
            config::SoftwareConnection::Obs(o) => &o.collections,
        };

        let collections = match collections {
            Some(c) => c,
            None => {
                self.send(t!(
                    "collection.error",
                    locale = &self.lang,
                    collection = name
                ))
                .await;
                return;
            }
        };

        let collection = match collections.get(&name) {
            Some(c) => c,
            None => {
                self.send(t!(
                    "collection.notFound",
                    locale = &self.lang,
                    collection = name
                ))
                .await;
                return;
            }
        };

        let bsc = match &state.broadcasting_software.connection {
            Some(b) => b,
            None => return,
        };

        if (bsc.set_collection_and_profile(collection).await).is_err() {
            self.send(t!(
                "collection.error",
                locale = &self.lang,
                collection = name
            ))
            .await;
            return;
        };

        self.send(t!(
            "collection.success",
            locale = &self.lang,
            collection = name
        ))
        .await;

        if state.broadcasting_software.is_streaming {
            self.send(t!("collection.note", locale = &self.lang,)).await;
        }
    }
}

fn condition_to_text(condition: bool, lang: &str) -> String {
    if condition {
        t!("handleCommands.enabled", locale = lang)
    } else {
        t!("handleCommands.disabled", locale = lang)
    }
}

fn enabled_to_bool(enabled: &str) -> Result<bool, error::Error> {
    if enabled.to_lowercase() == "on" {
        return Ok(true);
    }

    if enabled.to_lowercase() == "off" {
        return Ok(false);
    }

    Err(error::Error::EnabledToBoolConversionError)
}

async fn bitrate_msg(user: &Noalbs, lang: &str) -> String {
    let mut msg = String::new();

    let state = &user.state.read().await;
    let servers = &state.config.switcher.stream_servers;

    for (i, s) in servers.iter().enumerate().filter(|(_, s)| s.enabled) {
        let t = s.stream_server.bitrate().await;
        let sep = if i == 0 || msg.is_empty() { "" } else { " - " };

        if let Some(bitrate_message) = t.message {
            let locale = t!(
                "bitrate.success",
                name = &s.name,
                message = &bitrate_message
            );
            let _ = write!(msg, "{}{}", sep, locale);
        }
    }

    if msg.is_empty() {
        return t!("bitrate.error", locale = lang);
    }

    msg
}

#[derive(Debug)]
pub struct Timeout {
    pub channel: String,
    pub time: std::time::Instant,
}

const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const USHER_BASE: &str = "https://usher.ttvnw.net";
const GQL_BASE: &str = "https://gql.twitch.tv/gql";

// TODO: Check if not an ad?
async fn check_if_transcoding(channel: &str) -> Result<bool, error::Error> {
    let req_string = r#"{"query": "{streamPlaybackAccessToken(channelName: \"%USER%\",params: {platform: \"web\",playerBackend: \"mediaplayer\",playerType: \"site\"}){value signature}}"}"#;
    let req_string = req_string.replace("%USER%", channel);

    let client = reqwest::Client::new();
    let res = client
        .post(GQL_BASE)
        .header("Client-ID", CLIENT_ID)
        .body(req_string)
        .send()
        .await?;

    let json = res.json::<serde_json::Value>().await?;
    let json = json["data"]["streamPlaybackAccessToken"].to_owned();
    let json: StreamPlaybackAccessToken = serde_json::from_value(json)?;

    use rand::Rng;
    let rng = rand::thread_rng().gen_range(1000000..10000000);
    let query = M3u8Query {
        allow_source: String::from("true"),
        allow_audio_only: String::from("true"),
        allow_spectre: String::from("true"),
        p: rng,
        player: String::from("twitchweb"),
        playlist_include_framerate: String::from("true"),
        segment_preference: String::from("4"),
        sig: json.signature,
        token: json.value,
    };

    let res = client
        .get(format!("{}/api/channel/hls/{}.m3u8", USHER_BASE, channel))
        .header("Client-ID", CLIENT_ID)
        .query(&query)
        .send()
        .await?;

    let text = res.text().await?;
    // println!("Response:\n{}", text);

    if text.contains("TRANSCODESTACK=\"transmux\"")
        || text.contains("Can not find channel")
        || text.contains("transcode_does_not_exist")
    {
        return Ok(false);
    }

    Ok(true)
}

#[derive(Debug, Serialize)]
struct M3u8Query {
    allow_source: String,
    allow_audio_only: String,
    allow_spectre: String,
    p: u32,
    player: String,
    playlist_include_framerate: String,
    segment_preference: String,
    sig: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct StreamPlaybackAccessToken {
    value: String,
    signature: String,
}
