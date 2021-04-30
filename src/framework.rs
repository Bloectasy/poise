//! The central Framework struct that ties everything together.

use crate::serenity_prelude as serenity;
use crate::*;

pub struct Framework<U, E> {
    prefix: &'static str,
    user_data: once_cell::sync::OnceCell<U>,
    user_data_setup: std::sync::Mutex<
        Option<
            Box<
                dyn Send
                    + Sync
                    + for<'a> FnOnce(
                        &'a serenity::Context,
                        &'a serenity::Ready,
                        &'a Self,
                    ) -> BoxFuture<'a, Result<U, E>>,
            >,
        >,
    >,
    bot_id: std::sync::Mutex<Option<serenity::UserId>>,
    // TODO: wrap in RwLock to allow changing framework options while running? Could also replace
    // the edit tracking cache interior mutability
    options: FrameworkOptions<U, E>,
    application_id: serenity::ApplicationId,
}

impl<U, E> Framework<U, E> {
    /// Setup a new blank Framework with a prefix and a callback to provide user data.
    ///
    /// The user data callback is invoked as soon as the bot is logged. That way, bot data like user
    /// ID or connected guilds can be made available to the user data setup function. The user data
    /// setup is not allowed to return Result because there would be no reasonable
    /// course of action on error.
    pub fn new<F>(
        prefix: &'static str,
        application_id: serenity::ApplicationId,
        user_data_setup: F,
        options: FrameworkOptions<U, E>,
    ) -> Self
    where
        F: Send
            + Sync
            + 'static
            + for<'a> FnOnce(
                &'a serenity::Context,
                &'a serenity::Ready,
                &'a Self,
            ) -> BoxFuture<'a, Result<U, E>>,
    {
        Self {
            prefix,
            user_data: once_cell::sync::OnceCell::new(),
            user_data_setup: std::sync::Mutex::new(Some(Box::new(user_data_setup))),
            bot_id: std::sync::Mutex::new(None),
            options,
            application_id,
        }
    }

    pub async fn start(self, builder: serenity::ClientBuilder<'_>) -> Result<(), serenity::Error>
    where
        U: Send + Sync + 'static,
        E: 'static + Send,
    {
        let application_id = self.application_id;

        let self_1 = std::sync::Arc::new(self);
        let self_2 = std::sync::Arc::clone(&self_1);

        let edit_track_cache_purge_task = tokio::spawn(async move {
            loop {
                if let Some(edit_tracker) = &self_1.options.prefix_options.edit_tracker {
                    edit_tracker.write().purge();
                }
                // not sure if the purging interval should be configurable
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        let event_handler = EventWrapper(move |ctx, event| {
            let self_2 = std::sync::Arc::clone(&self_2);
            Box::pin(async move {
                self_2.event(ctx, event).await;
            }) as _
        });
        builder
            .application_id(application_id.0)
            .event_handler(event_handler)
            .await?
            .start()
            .await?;

        edit_track_cache_purge_task.abort();

        Ok(())
    }

    pub fn options(&self) -> &FrameworkOptions<U, E> {
        &self.options
    }

    pub fn application_id(&self) -> serenity::ApplicationId {
        self.application_id
    }

    // Commented out because it feels to specialized, and users will want to insert extra
    // bookkeeping anyways (e.g. number of slash commands, slash command names added, etc)

    // pub async fn register_slash_commands_in_guild(
    //     &self,
    //     http: &serenity::Http,
    //     guild_id: serenity::GuildId,
    // ) -> Result<(), serenity::Error> {
    //     for slash_cmd in &self.options.slash_options.commands {
    //         slash_cmd.create_in_guild(http, guild_id).await?;
    //     }
    //     Ok(())
    // }

    // pub async fn register_slash_commands_global(
    //     &self,
    //     http: &serenity::Http,
    // ) -> Result<(), serenity::Error> {
    //     for slash_cmd in &self.options.slash_options.commands {
    //         slash_cmd.create_global(http).await?;
    //     }
    //     Ok(())
    // }

    async fn get_user_data(&self) -> &U {
        // We shouldn't get a Message event before a Ready event. But if we do, wait until
        // the Ready event does come and the resulting data has arrived.
        loop {
            match self.user_data.get() {
                Some(x) => break x,
                None => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }

    /// Returns
    /// - Ok(()) if a command was successfully dispatched and run
    /// - Err(None) if the message does not match any known command
    /// - Err(Some(error: UserError)) if any user code yielded an error
    async fn dispatch_message<'a>(
        &'a self,
        ctx: prefix::PrefixContext<'a, U, E>,
        triggered_by_edit: bool,
    ) -> Result<(), Option<(E, PrefixCommandErrorContext<'a, U, E>)>> {
        // Check prefix
        let msg = match ctx.msg.content.strip_prefix(self.prefix) {
            Some(msg) => msg,
            None => self
                .options
                .prefix_options
                .additional_prefixes
                .iter()
                .find_map(|prefix| ctx.msg.content.strip_prefix(prefix))
                .ok_or(None)?,
        };

        // If we know our own ID, and the message author ID is our own, and we aren't supposed to
        // execute our own messages, THEN stop execution.
        if !self.options.prefix_options.execute_self_messages
            && *self.bot_id.lock().unwrap() == Some(ctx.msg.author.id)
        {
            return Err(None);
        }

        // Extract command name and arguments string
        let (command_name, args) = {
            let mut iter = msg.splitn(2, char::is_whitespace);
            (iter.next().unwrap(), iter.next().unwrap_or("").trim_start())
        };

        // Find the first matching command
        let mut first_matching_command = None;
        for command in &self.options.prefix_options.commands {
            if command.name != command_name {
                continue;
            }
            match (command
                .options
                .check
                .unwrap_or(self.options.prefix_options.command_check))(ctx)
            .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    return Err(Some((
                        e,
                        prefix::PrefixCommandErrorContext {
                            command,
                            ctx,
                            while_checking: true,
                        },
                    )));
                }
            }

            first_matching_command = Some(command);
            break;
        }
        let command = first_matching_command.ok_or(None)?;

        if triggered_by_edit && !command.options.track_edits {
            return Err(None);
        }

        // Typing is broadcasted as long as this object is alive
        let _typing_broadcaster = if command
            .options
            .broadcast_typing
            .unwrap_or(self.options.prefix_options.broadcast_typing)
        {
            match ctx.msg.channel_id.start_typing(&ctx.discord.http) {
                Ok(x) => Some(x),
                Err(e) => {
                    println!(
                        "Warning: couldn't start typing broadcast before command: {}",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        // Execute command
        (command.action)(ctx, args).await.map_err(|e| {
            Some((
                e,
                prefix::PrefixCommandErrorContext {
                    ctx,
                    command,
                    while_checking: false,
                },
            ))
        })
    }

    async fn dispatch_interaction(
        &self,
        ctx: SlashContext<'_, U, E>,
        name: &str,
        options: &[serenity::ApplicationCommandInteractionDataOption],
    ) {
        let command = match self
            .options
            .slash_options
            .commands
            .iter()
            .find(|cmd| cmd.name == name)
        {
            Some(x) => x,
            None => {
                println!("Warning: received unknown interaction \"{}\"", name);
                return;
            }
        };

        if command
            .options
            .defer_response
            .unwrap_or(self.options.slash_options.defer_response)
        {
            if let Err(e) = ctx.defer_response().await {
                println!("Failed to send interaction acknowledgement: {}", e);
            }
        }

        if let Err(e) = (command.action)(ctx, options).await {
            let error_ctx = SlashCommandErrorContext {
                command,
                ctx,
                while_checking: false,
            };
            if let Some(on_error) = command.options.on_error {
                on_error(e, error_ctx).await;
            } else {
                (self.options.on_error)(
                    e,
                    ErrorContext::Command(CommandErrorContext::Slash(error_ctx)),
                )
                .await;
            }
        }
    }

    async fn event(&self, ctx: serenity::Context, event: Event<'_>) {
        match &event {
            Event::Ready { data_about_bot } => {
                let user_data_setup = self.user_data_setup.lock().unwrap().take();
                if let Some(user_data_setup) = user_data_setup {
                    *self.bot_id.lock().unwrap() = Some(data_about_bot.user.id);
                    match user_data_setup(&ctx, &data_about_bot, self).await {
                        Ok(user_data) => {
                            let _: Result<_, _> = self.user_data.set(user_data);
                        }
                        Err(e) => (self.options.on_error)(e, ErrorContext::Setup).await,
                    }
                } else {
                    println!("Warning: skipping duplicate Discord bot ready event");
                }
            }
            Event::Message { new_message } => {
                let ctx = prefix::PrefixContext {
                    discord: &ctx,
                    msg: &new_message,
                    framework: self,
                    data: self.get_user_data().await,
                };
                if let Err(Some((err, err_ctx))) = self.dispatch_message(ctx, false).await {
                    if let Some(on_error) = err_ctx.command.options.on_error {
                        (on_error)(err, err_ctx).await;
                    } else {
                        (self.options.on_error)(
                            err,
                            crate::ErrorContext::Command(crate::CommandErrorContext::Prefix(
                                err_ctx,
                            )),
                        )
                        .await;
                    }
                }
            }
            Event::MessageUpdate { event, .. } => {
                if let Some(edit_tracker) = &self.options.prefix_options.edit_tracker {
                    let msg = edit_tracker.write().process_message_update(event);

                    let ctx = prefix::PrefixContext {
                        discord: &ctx,
                        msg: &msg,
                        framework: self,
                        data: self.get_user_data().await,
                    };
                    if let Err(Some((err, err_ctx))) = self.dispatch_message(ctx, true).await {
                        (self.options.on_error)(
                            err,
                            crate::ErrorContext::Command(crate::CommandErrorContext::Prefix(
                                err_ctx,
                            )),
                        );
                    }
                }
            }
            Event::MessageDelete {
                deleted_message_id, ..
            } => {
                if let Some(edit_tracker) = &self.options.prefix_options.edit_tracker {
                    let bot_response = edit_tracker
                        .write()
                        .find_bot_response(*deleted_message_id)
                        .cloned();
                    if let Some(bot_response) = bot_response {
                        if let Err(e) = bot_response.delete(&ctx).await {
                            println!(
                                "Warning: couldn't delete bot response when user deleted message: {}",
                                e
                            );
                        }
                    }
                }
            }
            Event::InteractionCreate { interaction } => {
                if let Some(data) = &interaction.data {
                    let has_sent_initial_response = std::sync::Mutex::new(false);
                    let slash_ctx = SlashContext {
                        data: self.get_user_data().await,
                        discord: &ctx,
                        framework: self,
                        interaction,
                        has_sent_initial_response: &has_sent_initial_response,
                    };
                    self.dispatch_interaction(slash_ctx, &data.name, &data.options)
                        .await;
                } else {
                    println!("Warning: interaction has no data");
                }
            }
            _ => {}
        }

        // Do this after the framework's Ready handling, so that self.get_user_data() doesnt
        // potentially block infinitely
        if let Err(e) =
            (self.options.listener)(&ctx, &event, self, self.get_user_data().await).await
        {
            (self.options.on_error)(e, ErrorContext::Listener(&event));
        }
    }
}