use crate::formula::clmm::concentrated_liquidity::compute_swap_step;
use crate::formula::clmm::constant::{ORCA_MAX_SQRT_PRICE_X64, MIN_SQRT_PRICE_X64};
use crate::formula::clmm::orca_swap_state::{checked_mul_div, next_tick_cross_update, NO_EXPLICIT_SQRT_PRICE_LIMIT, NUM_REWARDS, PostSwapUpdate, PROTOCOL_FEE_RATE_MUL_VALUE, Q64_RESOLUTION, SwapTickSequence, Tick, TICK_ARRAY_SIZE, TickUpdate};
use crate::formula::clmm::orca_tick_math::{sqrt_price_from_tick_index, tick_index_from_sqrt_price};
use crate::formula::clmm::raydium_swap_state::add_delta;
use crate::r#struct::pools::{OrcaClmmMarket, WhirlpoolRewardInfo};

pub fn swap_internal(
    whirlpool: &OrcaClmmMarket,
    swap_tick_sequence: &mut SwapTickSequence,
    amount: u64,
    sqrt_price_limit: u128,
    amount_specified_is_input: bool,
    a_to_b: bool,
    timestamp: u64,
) -> Result<PostSwapUpdate, &'static str> {
    let adjusted_sqrt_price_limit = if sqrt_price_limit == NO_EXPLICIT_SQRT_PRICE_LIMIT {
        if a_to_b {
            MIN_SQRT_PRICE_X64
        } else {
            ORCA_MAX_SQRT_PRICE_X64
        }
    } else {
        sqrt_price_limit
    };

    if !(MIN_SQRT_PRICE_X64..=ORCA_MAX_SQRT_PRICE_X64).contains(&adjusted_sqrt_price_limit) {
        return Err("ErrorCode::SqrtPriceOutOfBounds");
    }

    if a_to_b && adjusted_sqrt_price_limit > whirlpool.sqrt_price
        || !a_to_b && adjusted_sqrt_price_limit < whirlpool.sqrt_price
    {
        return Err("ErrorCode::InvalidSqrtPriceLimitDirection");
    }

    if amount == 0 {
        return Err("ErrorCode::ZeroTradableAmount");
    }

    let tick_spacing = whirlpool.tick_spacing;
    let fee_rate = whirlpool.fee_rate;
    let protocol_fee_rate = whirlpool.protocol_fee_rate;
    let next_reward_infos = next_whirlpool_reward_infos(whirlpool, timestamp)?;

    let mut amount_remaining: u64 = amount;
    let mut amount_calculated: u64 = 0;
    let mut curr_sqrt_price = whirlpool.sqrt_price;
    let mut curr_tick_index = whirlpool.tick_current_index;
    let mut curr_liquidity = whirlpool.liquidity;
    let mut curr_protocol_fee: u64 = 0;
    let mut curr_array_index: usize = 0;
    let mut curr_fee_growth_global_input = if a_to_b {
        whirlpool.fee_growth_global_a
    } else {
        whirlpool.fee_growth_global_b
    };

    while amount_remaining > 0 && adjusted_sqrt_price_limit != curr_sqrt_price {
        let (next_array_index, next_tick_index) = swap_tick_sequence
            .get_next_initialized_tick_index(
                curr_tick_index,
                tick_spacing,
                a_to_b,
                curr_array_index,
            )?;

        let (next_tick_sqrt_price, sqrt_price_target) =
            get_next_sqrt_prices(next_tick_index, adjusted_sqrt_price_limit, a_to_b);

        let swap_computation = compute_swap_step(
            curr_sqrt_price,
            sqrt_price_target,
            curr_liquidity,
            amount_remaining,
            fee_rate as u32,
            amount_specified_is_input,
            a_to_b,
        )?;

        if amount_specified_is_input {
            amount_remaining = amount_remaining
                .checked_sub(swap_computation.amount_in)
                .ok_or("ErrorCode::AmountRemainingOverflow")?;
            amount_remaining = amount_remaining
                .checked_sub(swap_computation.fee_amount)
                .ok_or("ErrorCode::AmountRemainingOverflow")?;

            amount_calculated = amount_calculated
                .checked_add(swap_computation.amount_out)
                .ok_or("ErrorCode::AmountCalcOverflow")?;
        } else {
            amount_remaining = amount_remaining
                .checked_sub(swap_computation.amount_out)
                .ok_or("ErrorCode::AmountRemainingOverflow")?;

            amount_calculated = amount_calculated
                .checked_add(swap_computation.amount_in)
                .ok_or("ErrorCode::AmountCalcOverflow")?;
            amount_calculated = amount_calculated
                .checked_add(swap_computation.fee_amount)
                .ok_or("ErrorCode::AmountCalcOverflow")?;
        }

        let (next_protocol_fee, next_fee_growth_global_input) = calculate_fees(
            swap_computation.fee_amount as u64,
            protocol_fee_rate,
            curr_liquidity,
            curr_protocol_fee,
            curr_fee_growth_global_input,
        );
        curr_protocol_fee = next_protocol_fee;
        curr_fee_growth_global_input = next_fee_growth_global_input;

        if swap_computation.sqrt_price_next_x64 == next_tick_sqrt_price {
            let (next_tick, next_tick_initialized) = swap_tick_sequence
                .get_tick(next_array_index, next_tick_index, tick_spacing)
                .map_or_else(|_| (None, false), |tick| (Some(tick), tick.initialized));

            if next_tick_initialized {
                let (fee_growth_global_a, fee_growth_global_b) = if a_to_b {
                    (curr_fee_growth_global_input, whirlpool.fee_growth_global_b)
                } else {
                    (whirlpool.fee_growth_global_a, curr_fee_growth_global_input)
                };

                // todo: for test only
                let (update, next_liquidity) = calculate_update(
                    next_tick.unwrap(),
                    a_to_b,
                    curr_liquidity,
                    fee_growth_global_a,
                    fee_growth_global_b,
                    &next_reward_infos,
                )?;
                // let next_liquidity = calculate_update(
                //     next_tick.unwrap(),
                //     a_to_b,
                //     curr_liquidity,
                //     fee_growth_global_a,
                //     fee_growth_global_b,
                // )?;

                curr_liquidity = next_liquidity;
                // todo: for test only
                swap_tick_sequence.update_tick(
                    next_array_index,
                    next_tick_index,
                    tick_spacing,
                    &update,
                )?;
            }

            let tick_offset = swap_tick_sequence.get_tick_offset(
                next_array_index,
                next_tick_index,
                tick_spacing,
            )?;

            curr_array_index = if (a_to_b && tick_offset == 0)
                || (!a_to_b && tick_offset == TICK_ARRAY_SIZE as isize - 1)
            {
                next_array_index + 1
            } else {
                next_array_index
            };

            curr_tick_index = if a_to_b {
                next_tick_index - 1
            } else {
                next_tick_index
            };
        } else if swap_computation.sqrt_price_next_x64 != curr_sqrt_price {
            curr_tick_index = tick_index_from_sqrt_price(&swap_computation.sqrt_price_next_x64);
        }

        curr_sqrt_price = swap_computation.sqrt_price_next_x64;
    }

    if amount_remaining > 0 && !amount_specified_is_input && sqrt_price_limit == NO_EXPLICIT_SQRT_PRICE_LIMIT {
        return Err("ErrorCode::PartialFillError");
    }

    let (amount_a, amount_b) = if a_to_b == amount_specified_is_input {
        (amount - amount_remaining, amount_calculated)
    } else {
        (amount_calculated, amount - amount_remaining)
    };

    Ok(PostSwapUpdate {
        amount_a,
        amount_b,
        next_liquidity: curr_liquidity,
        next_tick_index: curr_tick_index,
        next_sqrt_price: curr_sqrt_price,
        next_fee_growth_global: curr_fee_growth_global_input,
        next_reward_infos: [WhirlpoolRewardInfo::default(); 3],
        next_protocol_fee: curr_protocol_fee,
    })
}

fn get_next_sqrt_prices(
    next_tick_index: i32,
    sqrt_price_limit: u128,
    a_to_b: bool,
) -> (u128, u128) {
    let next_tick_price = sqrt_price_from_tick_index(next_tick_index);
    let next_sqrt_price_limit = if a_to_b {
        sqrt_price_limit.max(next_tick_price)
    } else {
        sqrt_price_limit.min(next_tick_price)
    };
    (next_tick_price, next_sqrt_price_limit)
}

// todo: for test only
fn calculate_update(
    tick: &Tick,
    a_to_b: bool,
    liquidity: u128,
    fee_growth_global_a: u128,
    fee_growth_global_b: u128,
    reward_infos: &[WhirlpoolRewardInfo; NUM_REWARDS],
) -> Result<(TickUpdate, u128), &'static str> {
// ) -> Result<u128, &'static str> {
    let signed_liquidity_net = if a_to_b {
        -tick.liquidity_net
    } else {
        tick.liquidity_net
    };

    let update =
        next_tick_cross_update(tick, fee_growth_global_a, fee_growth_global_b, reward_infos)?;

    // Update the global liquidity to reflect the new current tick
    // let next_liquidity = add_liquidity_delta(liquidity, signed_liquidity_net)?;
    let next_liquidity = add_delta(liquidity, signed_liquidity_net)?;

    Ok((update, next_liquidity))
    // Ok(next_liquidity)
}

fn calculate_fees(
    fee_amount: u64,
    protocol_fee_rate: u16,
    curr_liquidity: u128,
    curr_protocol_fee: u64,
    curr_fee_growth_global_input: u128,
) -> (u64, u128) {
    let mut next_protocol_fee = curr_protocol_fee;
    let mut next_fee_growth_global_input = curr_fee_growth_global_input;
    let mut global_fee = fee_amount;
    if protocol_fee_rate > 0 {
        let delta = calculate_protocol_fee(global_fee, protocol_fee_rate);
        global_fee -= delta;
        next_protocol_fee = next_protocol_fee.wrapping_add(delta);
    }

    if curr_liquidity > 0 {
        next_fee_growth_global_input = next_fee_growth_global_input
            .wrapping_add(((global_fee as u128) << Q64_RESOLUTION) / curr_liquidity);
    }
    (next_protocol_fee, next_fee_growth_global_input)
}

fn calculate_protocol_fee(global_fee: u64, protocol_fee_rate: u16) -> u64 {
    ((global_fee as u128) * (protocol_fee_rate as u128) / PROTOCOL_FEE_RATE_MUL_VALUE)
        .try_into()
        .unwrap()
}

pub fn next_whirlpool_reward_infos(
    whirlpool: &OrcaClmmMarket,
    next_timestamp: u64,
) -> Result<[WhirlpoolRewardInfo; NUM_REWARDS], &'static str> {
    let curr_timestamp = whirlpool.reward_last_updated_timestamp;
    if next_timestamp < curr_timestamp {
        return Err("ErrorCode::InvalidTimestamp");
    }

    if whirlpool.liquidity == 0 || next_timestamp == curr_timestamp {
        return Ok(whirlpool.reward_infos);
    }

    let mut next_reward_infos = whirlpool.reward_infos;
    let time_delta = u128::from(next_timestamp - curr_timestamp);
    for reward_info in next_reward_infos.iter_mut() {
        if !reward_info.initialized() {
            continue;
        }

        let reward_growth_delta = checked_mul_div(
            time_delta,
            reward_info.emissions_per_second_x64,
            whirlpool.liquidity,
        )
            .unwrap_or(0);

        let curr_growth_global = reward_info.growth_global_x64;
        reward_info.growth_global_x64 = curr_growth_global.wrapping_add(reward_growth_delta);
    }

    Ok(next_reward_infos)
}