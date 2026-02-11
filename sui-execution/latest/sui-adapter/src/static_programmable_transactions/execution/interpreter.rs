// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    execution_mode::ExecutionMode,
    gas_charger::GasCharger,
    sp,
    static_programmable_transactions::{
        env::Env,
        execution::context::{Context, CtxValue},
        execution::trace_utils,
        typing::ast as T,
    },
};
use move_binary_format::call_trace::{CallTraces, GasInfo, InputValue, InternalCallTrace};
use move_core_types::{account_address::AccountAddress, annotated_value as A, ident_str};
use move_trace_format::format::MoveTraceBuilder;
use move_vm_types::values::Value as VMValue;
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Instant};
use sui_types::{
    SUI_FRAMEWORK_ADDRESS,
    base_types::TxContext,
    coin::Coin,
    error::{ExecutionError, ExecutionErrorKind},
    execution::{ExecutionTiming, ResultWithTimings},
    execution_status::PackageUpgradeError,
    metrics::LimitsMetrics,
    move_package::MovePackage,
    object::Owner,
};
use tracing::instrument;

pub fn execute<'env, 'pc, 'vm, 'state, 'linkage, Mode: ExecutionMode>(
    env: &'env mut Env<'pc, 'vm, 'state, 'linkage>,
    metrics: Arc<LimitsMetrics>,
    tx_context: Rc<RefCell<TxContext>>,
    gas_charger: &mut GasCharger,
    ast: T::Transaction,
    trace_builder_opt: &mut Option<MoveTraceBuilder>,
) -> ResultWithTimings<Mode::ExecutionResults, ExecutionError>
where
    'pc: 'state,
    'env: 'state,
{
    let mut timings = vec![];
    let result = execute_inner::<Mode>(
        &mut timings,
        env,
        metrics,
        tx_context,
        gas_charger,
        ast,
        trace_builder_opt,
    );

    match result {
        Ok(result) => Ok((result, timings)),
        Err(e) => {
            trace_utils::trace_execution_error(trace_builder_opt, e.to_string());
            Err((e, timings))
        }
    }
}

pub fn execute_inner<'env, 'pc, 'vm, 'state, 'linkage, Mode: ExecutionMode>(
    timings: &mut Vec<ExecutionTiming>,
    env: &'env mut Env<'pc, 'vm, 'state, 'linkage>,
    metrics: Arc<LimitsMetrics>,
    tx_context: Rc<RefCell<TxContext>>,
    gas_charger: &mut GasCharger,
    ast: T::Transaction,
    trace_builder_opt: &mut Option<MoveTraceBuilder>,
) -> Result<Mode::ExecutionResults, ExecutionError>
where
    'pc: 'state,
{
    debug_assert_eq!(gas_charger.move_gas_status().stack_height_current(), 0);
    let T::Transaction {
        bytes,
        objects,
        withdrawals,
        pure,
        receiving,
        withdrawal_compatibility_conversions: _,
        commands,
    } = ast;
    let mut context = Context::new(
        env,
        metrics,
        tx_context,
        gas_charger,
        bytes,
        objects,
        withdrawals,
        pure,
        receiving,
    )?;

    trace_utils::trace_ptb_summary(&mut context, trace_builder_opt, &commands)?;

    let mut mode_results = Mode::empty_results();
    for sp!(idx, c) in commands {
        let start = Instant::now();
        if let Err(err) =
            execute_command::<Mode>(&mut context, &mut mode_results, c, trace_builder_opt)
        {
            if Mode::get_call_trace() {
                // In call-trace mode, keep collected trace results and stop executing more commands.
                break;
            }
            let object_runtime = context.object_runtime()?;
            // We still need to record the loaded child objects for replay
            let loaded_runtime_objects = object_runtime.loaded_runtime_objects();
            // we do not save the wrapped objects since on error, they should not be modified
            drop(context);
            // TODO wtf is going on with the borrow checker here. 'state is bound into the object
            // runtime, but its since been dropped. what gives with this error?
            env.state_view
                .save_loaded_runtime_objects(loaded_runtime_objects);
            timings.push(ExecutionTiming::Abort(start.elapsed()));
            return Err(err.with_command_index(idx as usize));
        };
        timings.push(ExecutionTiming::Success(start.elapsed()));
    }
    // Save loaded objects table in case we fail in post execution
    let object_runtime = context.object_runtime()?;
    // We still need to record the loaded child objects for replay
    // Record the objects loaded at runtime (dynamic fields + received) for
    // storage rebate calculation.
    let loaded_runtime_objects = object_runtime.loaded_runtime_objects();
    // We record what objects were contained in at the start of the transaction
    // for expensive invariant checks
    let wrapped_object_containers = object_runtime.wrapped_object_containers();
    // We record the generated object IDs for expensive invariant checks
    let generated_object_ids = object_runtime.generated_object_ids();

    // apply changes
    let finished = context.finish::<Mode>();
    // Save loaded objects for debug. We dont want to lose the info
    env.state_view
        .save_loaded_runtime_objects(loaded_runtime_objects);
    env.state_view
        .save_wrapped_object_containers(wrapped_object_containers);
    env.state_view.record_execution_results(finished?)?;
    env.state_view
        .record_generated_object_ids(generated_object_ids);
    Ok(mode_results)
}

/// Execute a single command
#[instrument(level = "trace", skip_all)]
fn execute_command<Mode: ExecutionMode>(
    context: &mut Context,
    mode_results: &mut Mode::ExecutionResults,
    c: T::Command_,
    trace_builder_opt: &mut Option<MoveTraceBuilder>,
) -> Result<(), ExecutionError> {
    let T::Command_ {
        command,
        result_type,
        drop_values,
        consumed_shared_objects: _,
    } = c;
    assert_invariant!(
        context.gas_charger.move_gas_status().stack_height_current() == 0,
        "stack height did not start at 0"
    );
    let is_move_call = matches!(command, T::Command__::MoveCall(_));
    let num_args = command.arguments_len();
    let mut args_to_update = vec![];
    let mut trace_results = None;
    let result = match command {
        T::Command__::MoveCall(move_call) => {
            trace_utils::trace_move_call_start(trace_builder_opt);
            let T::MoveCall {
                function,
                arguments,
            } = *move_call;
            if Mode::TRACK_EXECUTION {
                args_to_update.extend(
                    arguments
                        .iter()
                        .filter(|arg| matches!(&arg.value.1, T::Type::Reference(/* mut */ true, _)))
                        .cloned(),
                )
            }
            let call_arguments = context.arguments(arguments.clone())?;
            let res = if Mode::get_call_trace() {
                let (return_values, call_traces) = context.vm_move_call_trace(
                    function,
                    call_arguments,
                    &arguments,
                    trace_builder_opt,
                )?;
                trace_results = Some(call_traces);
                Ok(return_values.unwrap_or_default())
            } else {
                context.vm_move_call(function, call_arguments, trace_builder_opt)
            };
            trace_utils::trace_move_call_end(trace_builder_opt);
            res?
        }
        T::Command__::TransferObjects(objects, recipient) => {
            let object_tys = objects
                .iter()
                .map(|sp!(_, (_, ty))| ty.clone())
                .collect::<Vec<_>>();
            let object_values: Vec<CtxValue> = context.arguments(objects)?;
            let recipient: AccountAddress = context.argument(recipient)?;
            assert_invariant!(
                object_values.len() == object_tys.len(),
                "object values and types mismatch"
            );
            trace_utils::trace_transfer(context, trace_builder_opt, &object_values, &object_tys)?;
            if Mode::get_call_trace() {
                trace_results = Some(transfer_objects_call_trace(
                    context,
                    &object_values,
                    &object_tys,
                    recipient,
                )?);
            }
            for (object_value, ty) in object_values.into_iter().zip(object_tys) {
                // TODO should we just call a Move function?
                let recipient = Owner::AddressOwner(recipient.into());
                context.transfer_object(recipient, ty, object_value)?;
            }
            vec![]
        }
        T::Command__::SplitCoins(ty, coin, amounts) => {
            let mut trace_values = vec![];
            // TODO should we just call a Move function?
            if Mode::TRACK_EXECUTION {
                args_to_update.push(coin.clone());
            }
            let coin_ref: CtxValue = context.argument(coin)?;
            let amount_values: Vec<u64> = context.arguments(amounts)?;
            let mut total: u64 = 0;
            for amount in &amount_values {
                let Some(new_total) = total.checked_add(*amount) else {
                    return Err(ExecutionError::from_kind(
                        ExecutionErrorKind::CoinBalanceOverflow,
                    ));
                };
                total = new_total;
            }
            trace_utils::add_move_value_info_from_ctx_value(
                context,
                trace_builder_opt,
                &mut trace_values,
                &ty,
                &coin_ref,
            )?;
            let coin_value = context.copy_value(&coin_ref)?.coin_ref_value()?;
            fp_ensure!(
                coin_value >= total,
                ExecutionError::new_with_source(
                    ExecutionErrorKind::InsufficientCoinBalance,
                    format!("balance: {coin_value} required: {total}")
                )
            );
            coin_ref.coin_ref_subtract_balance(total)?;
            let amounts = amount_values
                .into_iter()
                .map(|a| context.new_coin(a))
                .collect::<Result<Vec<_>, _>>()?;
            trace_utils::trace_split_coins(
                context,
                trace_builder_opt,
                &ty,
                trace_values,
                &amounts,
                total,
            )?;

            amounts
        }
        T::Command__::MergeCoins(ty, target, coins) => {
            let mut trace_values = vec![];
            // TODO should we just call a Move function?
            if Mode::TRACK_EXECUTION {
                args_to_update.push(target.clone());
            }
            let target_ref: CtxValue = context.argument(target)?;
            trace_utils::add_move_value_info_from_ctx_value(
                context,
                trace_builder_opt,
                &mut trace_values,
                &ty,
                &target_ref,
            )?;
            let coins = context.arguments(coins)?;
            let amounts = coins
                .into_iter()
                .map(|coin| {
                    trace_utils::add_move_value_info_from_ctx_value(
                        context,
                        trace_builder_opt,
                        &mut trace_values,
                        &ty,
                        &coin,
                    )?;
                    context.destroy_coin(coin)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut additional: u64 = 0;
            for amount in amounts {
                let Some(new_additional) = additional.checked_add(amount) else {
                    return Err(ExecutionError::from_kind(
                        ExecutionErrorKind::CoinBalanceOverflow,
                    ));
                };
                additional = new_additional;
            }
            let target_value = context.copy_value(&target_ref)?.coin_ref_value()?;
            fp_ensure!(
                target_value.checked_add(additional).is_some(),
                ExecutionError::from_kind(ExecutionErrorKind::CoinBalanceOverflow,)
            );
            target_ref.coin_ref_add_balance(additional)?;
            trace_utils::trace_merge_coins(
                context,
                trace_builder_opt,
                &ty,
                trace_values,
                additional,
            )?;
            vec![]
        }
        T::Command__::MakeMoveVec(ty, items) => {
            let items: Vec<CtxValue> = context.arguments(items)?;
            trace_utils::trace_make_move_vec(context, trace_builder_opt, &items, &ty)?;
            vec![CtxValue::vec_pack(ty, items)?]
        }
        T::Command__::Publish(module_bytes, dep_ids, linkage) => {
            trace_utils::trace_publish_event(trace_builder_opt)?;
            let modules =
                context.deserialize_modules(&module_bytes, /* is upgrade */ false)?;

            let runtime_id = context.publish_and_init_package::<Mode>(
                modules,
                &dep_ids,
                linkage,
                trace_builder_opt,
            )?;

            if <Mode>::packages_are_predefined() {
                // no upgrade cap for genesis modules
                std::vec![]
            } else {
                std::vec![context.new_upgrade_cap(runtime_id)?]
            }
        }
        T::Command__::Upgrade(
            module_bytes,
            dep_ids,
            current_package_id,
            upgrade_ticket,
            linkage,
        ) => {
            trace_utils::trace_upgrade_event(trace_builder_opt)?;
            let upgrade_ticket = context
                .argument::<CtxValue>(upgrade_ticket)?
                .into_upgrade_ticket()?;
            // Make sure the passed-in package ID matches the package ID in the `upgrade_ticket`.
            if current_package_id != upgrade_ticket.package.bytes {
                return Err(ExecutionError::from_kind(
                    ExecutionErrorKind::PackageUpgradeError {
                        upgrade_error: PackageUpgradeError::PackageIDDoesNotMatch {
                            package_id: current_package_id,
                            ticket_id: upgrade_ticket.package.bytes,
                        },
                    },
                ));
            }
            // deserialize modules and charge gas
            let modules = context.deserialize_modules(&module_bytes, /* is upgrade */ true)?;

            let computed_digest = MovePackage::compute_digest_for_modules_and_deps(
                &module_bytes,
                &dep_ids,
                /* hash_modules */ true,
            )
            .to_vec();
            if computed_digest != upgrade_ticket.digest {
                return Err(ExecutionError::from_kind(
                    ExecutionErrorKind::PackageUpgradeError {
                        upgrade_error: PackageUpgradeError::DigestDoesNotMatch {
                            digest: computed_digest,
                        },
                    },
                ));
            }

            let upgraded_package_id = context.upgrade(
                modules,
                &dep_ids,
                current_package_id,
                upgrade_ticket.policy,
                linkage,
            )?;

            vec![context.upgrade_receipt(upgrade_ticket, upgraded_package_id)]
        }
    };
    if Mode::TRACK_EXECUTION {
        let argument_updates = context.argument_updates(args_to_update)?;
        let command_result = context.tracked_results(&result, &result_type)?;
        Mode::finish_command_v2(mode_results, argument_updates, command_result)?;
    }
    if Mode::get_call_trace() {
        Mode::finish_command_trace_v2(mode_results, &trace_results)?;
        if let Some(trace_results) = &trace_results
            && trace_results
                .0
                .first()
                .and_then(|trace| trace.error.as_ref())
                .is_some()
        {
            return Err(ExecutionError::new_with_source(
                ExecutionErrorKind::VMInvariantViolation,
                "call trace recorded execution error".to_string(),
            ));
        }
    }
    assert_invariant!(
        result.len() == drop_values.len(),
        "result values and drop values mismatch"
    );
    context.charge_command(is_move_call, num_args, result.len())?;
    let result = result
        .into_iter()
        .zip(drop_values)
        .map(|(value, drop)| if !drop { Some(value) } else { None })
        .collect::<Vec<_>>();
    context.result(result)?;
    assert_invariant!(
        context.gas_charger.move_gas_status().stack_height_current() == 0,
        "stack height did not end at 0"
    );
    Ok(())
}

fn transfer_objects_call_trace(
    context: &mut Context,
    object_values: &[CtxValue],
    object_tys: &[T::Type],
    recipient: AccountAddress,
) -> Result<CallTraces, ExecutionError> {
    assert_invariant!(
        object_values.len() == object_tys.len(),
        "object values and types mismatch for transfer call trace"
    );
    let mut call_traces = CallTraces::new();
    let mut inputs = Vec::with_capacity(object_values.len() + 1);
    for (value, ty) in object_values.iter().zip(object_tys) {
        let layout = context.env.fully_annotated_layout(ty)?;
        let move_value = VMValue::as_annotated_move_value_for_tracing_only(
            value.inner_for_tracing().inner_for_tracing(),
            &layout,
        )
        .ok_or_else(|| {
            make_invariant_violation!(
                "Failed to convert transfer object to MoveValue for call trace"
            )
        })?;
        let type_tag: sui_types::TypeTag = ty
            .clone()
            .try_into()
            .map_err(|e| make_invariant_violation!("Failed to convert type for call trace: {e}"))?;
        let input = if let sui_types::TypeTag::Struct(struct_tag) = &type_tag {
            if Coin::is_coin(struct_tag) {
                let (coin_id, balance) = extract_coin_id_and_balance(&move_value)?;
                InputValue::MoveValue(A::MoveValue::Struct(A::MoveStruct::new(
                    *struct_tag.clone(),
                    vec![
                        (
                            ident_str!("id").to_owned(),
                            A::MoveValue::Address(coin_id),
                        ),
                        (
                            ident_str!("balance").to_owned(),
                            A::MoveValue::U64(balance),
                        ),
                    ],
                )))
            } else {
                InputValue::String(serde_json::to_string(&move_value).map_err(|e| {
                    make_invariant_violation!(
                        "Failed to serialize transfer object for call trace: {e}"
                    )
                })?)
            }
        } else {
            InputValue::String(serde_json::to_string(&move_value).map_err(|e| {
                make_invariant_violation!("Failed to serialize transfer object for call trace: {e}")
            })?)
        };
        inputs.push(Some(input));
    }
    inputs.push(Some(InputValue::MoveValue(A::MoveValue::Address(recipient))));
    let call_trace = InternalCallTrace {
        pc: 0,
        from_module_id: SUI_FRAMEWORK_ADDRESS.to_string(),
        module_id: SUI_FRAMEWORK_ADDRESS.to_string(),
        func_name: "transfer_objects".to_string(),
        inputs,
        outputs: vec![],
        type_args: vec![],
        sub_traces: CallTraces::new(),
        fdef_idx: 0,
        gas_info: GasInfo::make_frame(0),
        error: None,
    };
    call_traces
        .push(call_trace)
        .expect("Failed to push transfer call trace");
    Ok(call_traces)
}

fn extract_coin_id_and_balance(move_value: &A::MoveValue) -> Result<(AccountAddress, u64), ExecutionError> {
    let A::MoveValue::Struct(coin_struct) = move_value else {
        invariant_violation!("Expected coin value to be a struct");
    };
    let [(_, id_value), (_, balance_value)] = coin_struct.fields.as_slice() else {
        invariant_violation!("Expected coin struct to have two fields");
    };

    let coin_id = extract_coin_id_address(id_value)?;
    let balance = extract_coin_balance(balance_value)?;
    Ok((coin_id, balance))
}

fn extract_coin_id_address(id_value: &A::MoveValue) -> Result<AccountAddress, ExecutionError> {
    let A::MoveValue::Struct(uid_struct) = id_value else {
        invariant_violation!("Expected coin id field to be UID struct");
    };
    let [(_, id_inner)] = uid_struct.fields.as_slice() else {
        invariant_violation!("Expected UID struct to have one field");
    };
    let A::MoveValue::Struct(id_struct) = id_inner else {
        invariant_violation!("Expected UID inner value to be ID struct");
    };
    let [(_, bytes_value)] = id_struct.fields.as_slice() else {
        invariant_violation!("Expected ID struct to have one field");
    };
    let A::MoveValue::Address(address) = bytes_value else {
        invariant_violation!("Expected ID bytes field to be address");
    };
    Ok(*address)
}

fn extract_coin_balance(balance_value: &A::MoveValue) -> Result<u64, ExecutionError> {
    let A::MoveValue::Struct(balance_struct) = balance_value else {
        invariant_violation!("Expected coin balance field to be Balance struct");
    };
    let [(_, amount_value)] = balance_struct.fields.as_slice() else {
        invariant_violation!("Expected Balance struct to have one field");
    };
    let A::MoveValue::U64(amount) = amount_value else {
        invariant_violation!("Expected balance amount field to be u64");
    };
    Ok(*amount)
}
