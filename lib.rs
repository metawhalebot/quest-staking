use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    self, Mint, TokenAccount, TokenInterface, TransferChecked,
};

declare_id!("REPLACE_WITH_PROGRAM_ID");

const QUEST_MINT: Pubkey =
    pubkey!("3GSaK6GcisJZj3DsZY3VQN6pNz2hGQ1M6r1qs83DcUBn");

const QUEST_DECIMALS: u8 = 6;

const FOUR_MONTHS: i64 = 4 * 30 * 24 * 60 * 60;
const EIGHT_MONTHS: i64 = 8 * 30 * 24 * 60 * 60;
const TWELVE_MONTHS: i64 = 12 * 30 * 24 * 60 * 60;

const APY_4: u64 = 600;
const APY_8: u64 = 1200;
const APY_12: u64 = 1800;

const EARLY_PENALTY_BPS: u64 = 3000;
const BPS: u64 = 10_000;

#[program]
pub mod quest_staking {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        require!(
            ctx.accounts.quest_mint.key() == QUEST_MINT,
            ErrorCode::WrongMint
        );

        let config = &mut ctx.accounts.config;

        config.admin = ctx.accounts.admin.key();
        config.quest_mint = ctx.accounts.quest_mint.key();
        config.staking_vault = ctx.accounts.staking_vault.key();
        config.reward_vault = ctx.accounts.reward_vault.key();
        config.bump = ctx.bumps.config;

        Ok(())
    }

    pub fn stake(
        ctx: Context<Stake>,
        amount: u64,
        plan: u8,
        position_id: u64,
    ) -> Result<()> {
        require!(amount > 0, ErrorCode::InvalidAmount);
        require!((1..=3).contains(&plan), ErrorCode::InvalidPlan);

        let (lock_time, apy_bps) = match plan {
            1 => (FOUR_MONTHS, APY_4),
            2 => (EIGHT_MONTHS, APY_8),
            3 => (TWELVE_MONTHS, APY_12),
            _ => return err!(ErrorCode::InvalidPlan),
        };

        let clock = Clock::get()?;

        let transfer_accounts = TransferChecked {
            mint: ctx.accounts.quest_mint.to_account_info(),
            from: ctx.accounts.user_token.to_account_info(),
            to: ctx.accounts.staking_vault.to_account_info(),
            authority: ctx.accounts.user.to_account_info(),
        };

        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            transfer_accounts,
        );

        token_interface::transfer_checked(
            transfer_ctx,
            amount,
            QUEST_DECIMALS,
        )?;

        let position = &mut ctx.accounts.position;

        position.owner = ctx.accounts.user.key();
        position.position_id = position_id;
        position.amount = amount;
        position.plan = plan;
        position.apy_bps = apy_bps;
        position.start_time = clock.unix_timestamp;
        position.unlock_time = clock
            .unix_timestamp
            .checked_add(lock_time)
            .ok_or(ErrorCode::MathOverflow)?;
        position.active = true;
        position.bump = ctx.bumps.position;

        emit!(Staked {
            owner: position.owner,
            position_id,
            amount,
            plan,
            unlock_time: position.unlock_time,
        });

        Ok(())
    }

    pub fn unstake(ctx: Context<Unstake>) -> Result<()> {
        let clock = Clock::get()?;
        let position = &ctx.accounts.position;

        require!(position.active, ErrorCode::InactivePosition);
        require!(
            clock.unix_timestamp >= position.unlock_time,
            ErrorCode::StillLocked
        );

        let principal = position.amount;

        let duration = position
            .unlock_time
            .checked_sub(position.start_time)
            .ok_or(ErrorCode::MathOverflow)? as u64;

        let reward = calculate_reward(
            principal,
            position.apy_bps,
            duration,
        )?;

        transfer_from_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.quest_mint,
            &ctx.accounts.staking_vault,
            &ctx.accounts.user_token,
            &ctx.accounts.vault_authority,
            principal,
            &[b"vault-authority", &[ctx.bumps.vault_authority]],
        )?;

        if reward > 0 {
            transfer_from_vault(
                &ctx.accounts.token_program,
                &ctx.accounts.quest_mint,
                &ctx.accounts.reward_vault,
                &ctx.accounts.user_token,
                &ctx.accounts.vault_authority,
                reward,
                &[b"vault-authority", &[ctx.bumps.vault_authority]],
            )?;
        }

        ctx.accounts.position.active = false;

        emit!(Unstaked {
            owner: position.owner,
            position_id: position.position_id,
            principal,
            reward,
        });

        Ok(())
    }

    pub fn early_unstake(ctx: Context<EarlyUnstake>) -> Result<()> {
        let position = &ctx.accounts.position;

        require!(position.active, ErrorCode::InactivePosition);

        let principal = position.amount;

        let penalty = principal
            .checked_mul(EARLY_PENALTY_BPS)
            .ok_or(ErrorCode::MathOverflow)?
            .checked_div(BPS)
            .ok_or(ErrorCode::MathOverflow)?;

        let returned = principal
            .checked_sub(penalty)
            .ok_or(ErrorCode::MathOverflow)?;

        // Early unstake: all accrued reward is forfeited.

        transfer_from_vault(
            &ctx.accounts.token_program,
            &ctx.accounts.quest_mint,
            &ctx.accounts.staking_vault,
            &ctx.accounts.user_token,
            &ctx.accounts.vault_authority,
            returned,
            &[b"vault-authority", &[ctx.bumps.vault_authority]],
        )?;

        ctx.accounts.position.active = false;

        emit!(EarlyUnstaked {
            owner: position.owner,
            position_id: position.position_id,
            principal,
            penalty,
            returned,
        });

        Ok(())
    }

    pub fn fund_rewards(
        ctx: Context<FundRewards>,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, ErrorCode::InvalidAmount);

        let transfer_accounts = TransferChecked {
            mint: ctx.accounts.quest_mint.to_account_info(),
            from: ctx.accounts.admin_token.to_account_info(),
            to: ctx.accounts.reward_vault.to_account_info(),
            authority: ctx.accounts.admin.to_account_info(),
        };

        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            transfer_accounts,
        );

        token_interface::transfer_checked(
            transfer_ctx,
            amount,
            QUEST_DECIMALS,
        )?;

        Ok(())
    }
}

fn calculate_reward(
    principal: u64,
    apy_bps: u64,
    seconds: u64,
) -> Result<u64> {
    let year_seconds: u64 = 365 * 24 * 60 * 60;

    let reward = (principal as u128)
        .checked_mul(apy_bps as u128)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_mul(seconds as u128)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(BPS as u128)
        .ok_or(ErrorCode::MathOverflow)?
        .checked_div(year_seconds as u128)
        .ok_or(ErrorCode::MathOverflow)?;

    require!(
        reward <= u64::MAX as u128,
        ErrorCode::MathOverflow
    );

    Ok(reward as u64)
}

fn transfer_from_vault<'info>(
    token_program: &Interface<'info, TokenInterface>,
    mint: &InterfaceAccount<'info, Mint>,
    from: &InterfaceAccount<'info, TokenAccount>,
    to: &InterfaceAccount<'info, TokenAccount>,
    authority: &UncheckedAccount<'info>,
    amount: u64,
    signer_seeds: &[&[u8]],
) -> Result<()> {
    let accounts = TransferChecked {
        mint: mint.to_account_info(),
        from: from.to_account_info(),
        to: to.to_account_info(),
        authority: authority.to_account_info(),
    };

    let ctx = CpiContext::new(
        token_program.to_account_info(),
        accounts,
    )
    .with_signer(&[signer_seeds]);

    token_interface::transfer_checked(
        ctx,
        amount,
        QUEST_DECIMALS,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(address = QUEST_MINT @ ErrorCode::WrongMint)]
    pub quest_mint: InterfaceAccount<'info, Mint>,

    #[account(
        init,
        payer = admin,
        space = 8 + Config::LEN,
        seeds = [b"config", quest_mint.key().as_ref()],
        bump
    )]
    pub config: Account<'info, Config>,

    #[account(
        init,
        payer = admin,
        token::mint = quest_mint,
        token::authority = vault_authority,
        token::token_program = token_program,
        seeds = [b"staking-vault"],
        bump
    )]
    pub staking_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        init,
        payer = admin,
        token::mint = quest_mint,
        token::authority = vault_authority,
        token::token_program = token_program,
        seeds = [b"reward-vault"],
        bump
    )]
    pub reward_vault: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: PDA authority for both vaults.
    #[account(
        seeds = [b"vault-authority"],
        bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(position_id: u64)]
pub struct Stake<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(address = QUEST_MINT @ ErrorCode::WrongMint)]
    pub quest_mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = user_token.owner == user.key(),
        constraint = user_token.mint == quest_mint.key()
    )]
    pub user_token: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"staking-vault"],
        bump
    )]
    pub staking_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        init,
        payer = user,
        space = 8 + StakePosition::LEN,
        seeds = [
            b"position",
            user.key().as_ref(),
            position_id.to_le_bytes().as_ref()
        ],
        bump
    )]
    pub position: Account<'info, StakePosition>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    pub user: Signer<'info>,

    #[account(address = QUEST_MINT @ ErrorCode::WrongMint)]
    pub quest_mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        has_one = owner @ ErrorCode::Unauthorized,
        constraint = position.active
    )]
    pub position: Account<'info, StakePosition>,

    /// CHECK: Must equal position.owner.
    #[account(address = position.owner)]
    pub owner: UncheckedAccount<'info>,

    #[account(
        mut,
        constraint = user_token.owner == user.key(),
        constraint = user_token.mint == quest_mint.key()
    )]
    pub user_token: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"staking-vault"],
        bump
    )]
    pub staking_vault: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"reward-vault"],
        bump
    )]
    pub reward_vault: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: PDA authority for vaults.
    #[account(
        seeds = [b"vault-authority"],
        bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct EarlyUnstake<'info> {
    pub user: Signer<'info>,

    #[account(address = QUEST_MINT @ ErrorCode::WrongMint)]
    pub quest_mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        has_one = owner @ ErrorCode::Unauthorized,
        constraint = position.active
    )]
    pub position: Account<'info, StakePosition>,

    /// CHECK: Must equal position.owner.
    #[account(address = position.owner)]
    pub owner: UncheckedAccount<'info>,

    #[account(
        mut,
        constraint = user_token.owner == user.key(),
        constraint = user_token.mint == quest_mint.key()
    )]
    pub user_token: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"staking-vault"],
        bump
    )]
    pub staking_vault: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: PDA authority for vault.
    #[account(
        seeds = [b"vault-authority"],
        bump
    )]
    pub vault_authority: UncheckedAccount<'info>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct FundRewards<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(address = QUEST_MINT @ ErrorCode::WrongMint)]
    pub quest_mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = admin_token.owner == admin.key(),
        constraint = admin_token.mint == quest_mint.key()
    )]
    pub admin_token: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"reward-vault"],
        bump
    )]
    pub reward_vault: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[account]
pub struct Config {
    pub admin: Pubkey,
    pub quest_mint: Pubkey,
    pub staking_vault: Pubkey,
    pub reward_vault: Pubkey,
    pub bump: u8,
}

impl Config {
    pub const LEN: usize = 32 + 32 + 32 + 32 + 1;
}

#[account]
pub struct StakePosition {
    pub owner: Pubkey,
    pub position_id: u64,
    pub amount: u64,
    pub plan: u8,
    pub apy_bps: u64,
    pub start_time: i64,
    pub unlock_time: i64,
    pub active: bool,
    pub bump: u8,
}

impl StakePosition {
    pub const LEN: usize = 32 + 8 + 8 + 1 + 8 + 8 + 8 + 1 + 1;
}

#[event]
pub struct Staked {
    pub owner: Pubkey,
    pub position_id: u64,
    pub amount: u64,
    pub plan: u8,
    pub unlock_time: i64,
}

#[event]
pub struct Unstaked {
    pub owner: Pubkey,
    pub position_id: u64,
    pub principal: u64,
    pub reward: u64,
}

#[event]
pub struct EarlyUnstaked {
    pub owner: Pubkey,
    pub position_id: u64,
    pub principal: u64,
    pub penalty: u64,
    pub returned: u64,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Wrong QUEST mint.")]
    WrongMint,

    #[msg("Invalid amount.")]
    InvalidAmount,

    #[msg("Invalid staking plan.")]
    InvalidPlan,

    #[msg("Stake is still locked.")]
    StillLocked,

    #[msg("Stake is inactive.")]
    InactivePosition,

    #[msg("Unauthorized user.")]
    Unauthorized,

    #[msg("Arithmetic overflow.")]
    MathOverflow,
}
