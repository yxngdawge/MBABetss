use anyhow::{Result, bail, Ok, anyhow};
use itertools::Itertools;
use serenity::{
    prelude::*, 
    model::{
        application::{
            component::{ButtonStyle, InputTextStyle, ActionRowComponent}, 
            interaction::{
                InteractionResponseType, application_command::ApplicationCommandInteraction, 
                message_component::MessageComponentInteraction, modal::ModalSubmitInteraction
            }
        }, 
        prelude::{GuildId, command::CommandOptionType, application_command::CommandDataOptionValue}
    }, http::Http
};
use serenity_utils::{BotUtil, MessageBuilder, Button};
use shellwords::split;
use crate::{betting_bot::BettingBot, config::config, serialize_utils::{BetOutcome, BetAction}, front_utils::{shorten, outcomes_display, bet_stub}};

impl BettingBot {
    fn bet_parse(
        command: &ApplicationCommandInteraction,
    ) -> Result<(String, Vec<String>)> {
        let desc = if let CommandDataOptionValue::String(value) = command
            .data
            .options
            .get(0)
            .expect("Expected a description of the bet")
            .resolved
            .as_ref()
            .expect("Expected a string")
        {
            value.clone()
        } else {
            String::new()
        };
        let outcomes = split(
            if let CommandDataOptionValue::String(value) = command
                .data
                .options
                .get(1)
                .expect("Expected outcomes for the bet")
                .resolved
                .as_ref()
                .expect("Expected a string")
            {
                value
            } else {
                ""
            },
        )?;
        Ok((desc, outcomes))
    }

    pub async fn bet_command(
        &self,
        ctx: Context,
        command: ApplicationCommandInteraction,
    ) -> Result<()> {
        let guild_id = command.guild_id.ok_or(anyhow!("command used outside a server"))?;
        let (desc, outcomes) = Self::bet_parse(&command)?;
        if outcomes.len() < 2 {
            ctx.http.answer(
                &command,
                MessageBuilder::new("You must define 2 outcomes or more to create a bet.")
            )
            .await?;
            bail!("Less than 2 ouctomes");
        }
        let bet_msg = ctx.http.answer(
            &command, 
            MessageBuilder::new(&desc).buttons(vec![
                Button { custom_id: BetAction::Lock().to_string(), label: "🔒 Lock".to_string(), style: ButtonStyle::Secondary },
                Button { custom_id: BetAction::Abort().to_string(), label: "🚫 Abort".to_string(), style: ButtonStyle::Danger }
            ])
        ).await?;
        let bet_uuid = bet_msg.id.0;
        self.bets.create_bet(bet_uuid, guild_id.0, command.user.id.0, desc, &outcomes)?;
        let outcome_displays = outcomes_display(&bet_stub(&outcomes));
        for (i, outcome) in outcome_displays.iter().enumerate() {
            let outcome_msg = ctx.http.send(bet_msg.channel_id, MessageBuilder::new(outcome).buttons(vec![
                Button { 
                    custom_id: BetAction::BetClick(BetOutcome { bet_id: bet_uuid, outcome_id: i }).to_string(), 
                    label: format!("{} Bet", config.currency), style: ButtonStyle::Primary 
                }
            ])).await?;
            self.msg_map.insert(BetOutcome {bet_id: bet_uuid, outcome_id: i}, outcome_msg.id.0)?;
        }
        Ok(())
    }

    pub async fn leaderboard_command(
        &self,
        ctx: Context,
        command: ApplicationCommandInteraction,
    ) -> Result<()> {
        let guild_id = command.guild_id.ok_or(anyhow!("command used outside a server"))?;
        let mut accounts = self.bets.accounts(guild_id.0)?;
        // sort by balance+inbet first and balance to tie break
        accounts.sort_by_key(|acc| (acc.balance+acc.in_bet, acc.balance));
        accounts.reverse();
        let msg = format!("{}  ({} in bet)   user\n", config.currency, config.currency) 
        + &accounts.into_iter().take(10).map(|acc| 
            format!("{}  ({})   <@{}>", acc.balance, acc.in_bet, acc.user)
        ).join("\n") + "\n...";
        ctx.http.answer(&command, MessageBuilder::new(msg)).await?;
        Ok(())
    }

    async fn is_admin(&self, command: &MessageComponentInteraction) -> Result<bool> {
        if let Some(member) = &command.user.member {
            let permissions = member.permissions.ok_or(anyhow!("couldn't get permissions"))?;
            return Ok(permissions.administrator());
        }
        bail!("couldn't get member");
    }

    pub async fn check_rights(&self, ctx: &Context, command: &MessageComponentInteraction, bet_id: u64) -> Result<()> {
        let user_uuid = command.user.id.0;
        let info = self.bets.get_info(bet_id)?;
        if info.author != user_uuid && !self.is_admin(command).await? {
            command.create_interaction_response(
                &ctx.http, |response| 
                response.kind(InteractionResponseType::ChannelMessageWithSource).interaction_response_data(
                    |answer| answer.ephemeral(true)
                    .content("Only the bet author or admins can perform this action")
                )
            ).await?;
            bail!("user is not bet author and not admin");
        }
        Ok(())
    }
    
    pub async fn lock_action(&self, ctx: Context, command: &MessageComponentInteraction, bet_id: u64) -> Result<()> {
        let server_uuid = command.guild_id.ok_or(anyhow!("action triggered outside server"))?.0;
        self.check_rights(&ctx, command, bet_id).await?;
        self.bets.lock_bet(bet_id)?;

        Ok(())
    }

    pub async fn abort_action(&self, ctx: Context, command: &MessageComponentInteraction, bet_id: u64) -> Result<()> {
        let server_uuid = command.guild_id.ok_or(anyhow!("action triggered outside server"))?.0;
        self.check_rights(&ctx, command, bet_id).await?;
        self.bets.abort_bet(bet_id)?;
        Ok(())
    }

    pub async fn bet_click_action(&self, ctx: Context, command: &MessageComponentInteraction, bet_outcome: BetOutcome) -> Result<()> {
        let server_uuid = command.guild_id.ok_or(anyhow!("action triggered outside server"))?.0;
        let user_uuid = command.user.id.0;
        let balance = self.balance_create(server_uuid, user_uuid)?;
        let bet_info = self.bets.get_info(bet_outcome.bet_id)?;
        command.create_interaction_response(
            &ctx.http, |response| 
            response.kind(InteractionResponseType::Modal).interaction_response_data(|modal|
                modal.custom_id(BetAction::BetOrder(user_uuid).to_string())
                    .title(format!("{} ({} {})", shorten(&bet_info.desc, 20), balance, config.currency))
                    .components(|act_row| {
                        act_row.create_action_row(|field| field.create_input_text(|input| {
                            input.custom_id(bet_outcome.to_string())
                                .style(InputTextStyle::Short)
                                .label(format!("How much to bet on:\n{}", shorten(&command.message.content, 30)))
                                .placeholder("100")
                                .required(true)
                        }))
                    })
            )).await?;
        Ok(())
    }

    pub async fn bet_order_action(&self, ctx: Context, command: &ModalSubmitInteraction, user: u64) -> Result<()> {
        if let ActionRowComponent::InputText(input) = &(&command.data.components[0]).components[0] {
            let bet_outcome = BetOutcome::try_from(input.custom_id.as_ref())?;
            let amount: u64 = input.value.parse()?;
            let (acc_update, bet) = self.bets.bet_on(bet_outcome.bet_id, bet_outcome.outcome_id, user, amount)?;
            command.create_interaction_response(
                &ctx.http, |response| 
                response.kind(InteractionResponseType::ChannelMessageWithSource).interaction_response_data(|answer|
                answer.ephemeral(true).content(format!(
                    "Succesfully bet {} {} on:\n{}\nnew balance: {} {}", 
                    amount, config.currency, bet.outcomes[bet_outcome.outcome_id].desc, acc_update.balance, config.currency
                )))
            ).await?;
            for (i, outcome) in outcomes_display(&bet).iter().enumerate() {
                let msg_id = self.msg_map.get(BetOutcome { bet_id: bet_outcome.bet_id, outcome_id: i })?;
                let mut msg = ctx.http.get_message(command.channel_id.0, msg_id).await?;
                msg.edit(&ctx.http, |msg| msg.content(outcome)).await?;
            }
        }
        Ok(())
    }

    pub async fn resolve_action(&self, ctx: Context, command: &MessageComponentInteraction, bet_outcome: BetOutcome) -> Result<()> {
        let server_uuid = command.guild_id.ok_or(anyhow!("action triggered outside server"))?.0;
        self.check_rights(&ctx, command, bet_outcome.bet_id).await?;
        self.bets.resolve(bet_outcome.bet_id, bet_outcome.outcome_id)?;
        Ok(())
    }

    pub async fn register_commands(&self, http: &Http, id: GuildId) {
        println!("Registering slash commands for Guild {}", id);
        if let Err(why) =
            GuildId::set_application_commands(&id, http, |commands| {
                commands
                    .create_application_command(|command| {
                        command
                            .name("bet")
                            .description("Create a bet.")
                            .create_option(|option| {
                                option
                                    .name("desc")
                                    .description("The description of the bet")
                                    .kind(CommandOptionType::String)
                                    .required(true)
                            })
                            .create_option(|option| {
                                option
                                    .name("options")
                                    .description("The possible outcomes of the bet")
                                    .kind(CommandOptionType::String)
                                    .required(true)
                            })
                    })
                    .create_application_command(|command| {
                        command
                            .name("leaderboard")
                            .description("Displays the leadeboard.")
                            .create_option(|option| {
                                option
                                    .name("permanent")
                                    .description("To make a ever updating leaderboard")
                                    .kind(CommandOptionType::Boolean)
                                    .required(false)
                            })
                    })
            })
            .await
        {
            println!("Couldn't register slash commmands: {}", why);
        };
    }
}