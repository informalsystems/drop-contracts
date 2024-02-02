use std::{str::FromStr, vec};

use cosmos_sdk_proto::cosmos::{
    bank::v1beta1::MsgSend,
    base::{abci::v1beta1::TxMsgData, v1beta1::Coin},
    staking::v1beta1::{MsgDelegate, MsgUndelegate},
};
use cosmwasm_std::{
    attr, ensure_eq, entry_point, to_json_binary, Addr, CosmosMsg, Deps, Reply, StdError, SubMsg,
    Uint128, WasmMsg,
};
use cosmwasm_std::{Binary, DepsMut, Env, MessageInfo, Response, StdResult};
use cw2::set_contract_version;
use lido_helpers::answer::response;
use lido_staking_base::{
    msg::puppeteer::{ExecuteMsg, InstantiateMsg, MigrateMsg, QueryExtMsg},
    state::puppeteer::{
        Config, KVQueryType, BALANCES, DELEGATIONS, SUDO_IBC_TRANSFER_REPLY_ID,
        SUDO_KV_BALANCE_REPLY_ID, SUDO_KV_DELEGATIONS_REPLY_ID, SUDO_PAYLOAD_REPLY_ID,
    },
};
use neutron_sdk::{
    bindings::{
        msg::{IbcFee, NeutronMsg},
        query::NeutronQuery,
        types::ProtobufAny,
    },
    interchain_queries::v045::{
        new_register_balance_query_msg, new_register_delegator_delegations_query_msg,
        types::{Balances, Delegations},
    },
    interchain_txs::helpers::decode_message_response,
    sudo::msg::{RequestPacket, RequestPacketTimeoutHeight, SudoMsg},
    NeutronError, NeutronResult,
};

use lido_puppeteer_base::{
    error::{ContractError, ContractResult},
    msg::{
        QueryMsg, ReceiverExecuteMsg, ResponseAnswer, ResponseHookErrorMsg, ResponseHookMsg,
        ResponseHookSuccessMsg, Transaction, TransferReadyBatchMsg,
    },
    proto::MsgIBCTransfer,
    state::{IcaState, PuppeteerBase, State, TxState, TxStateStatus, ICA_ID, LOCAL_DENOM},
};

use prost::Message;

use crate::{
    proto::cosmos::base::v1beta1::Coin as ProtoCoin,
    proto::liquidstaking::{
        distribution::v1beta1::MsgWithdrawDelegatorReward,
        staking::v1beta1::{
            MsgBeginRedelegate, MsgBeginRedelegateResponse, MsgDelegateResponse,
            MsgRedeemTokensforShares, MsgRedeemTokensforSharesResponse, MsgTokenizeShares,
            MsgTokenizeSharesResponse, MsgUndelegateResponse,
        },
    },
};
pub type Puppeteer<'a> = PuppeteerBase<'a, Config, KVQueryType>;

const CONTRACT_NAME: &str = concat!("crates.io:lido-neutron-contracts__", env!("CARGO_PKG_NAME"));
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_TIMEOUT_SECONDS: u64 = 60;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> NeutronResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    let owner = deps.api.addr_validate(&msg.owner)?;
    let allowed_senders = msg
        .allowed_senders
        .iter()
        .map(|addr| deps.api.addr_validate(addr))
        .collect::<StdResult<Vec<_>>>()?;
    let config = &Config {
        connection_id: msg.connection_id,
        port_id: msg.port_id,
        update_period: msg.update_period,
        remote_denom: msg.remote_denom,
        owner,
        allowed_senders,
        proxy_address: None,
        transfer_channel_id: msg.transfer_channel_id,
    };
    DELEGATIONS.save(
        deps.storage,
        &(
            Delegations {
                delegations: vec![],
            },
            0,
        ),
    )?;
    BALANCES.save(deps.storage, &(Balances { coins: vec![] }, 0))?;
    Puppeteer::default().instantiate(deps, config)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(
    deps: Deps<NeutronQuery>,
    env: Env,
    msg: QueryMsg<QueryExtMsg>,
) -> ContractResult<Binary> {
    match msg {
        QueryMsg::Extention { msg } => match msg {
            QueryExtMsg::Delegations {} => {
                to_json_binary(&DELEGATIONS.load(deps.storage)?).map_err(ContractError::Std)
            }
            QueryExtMsg::Balances {} => {
                to_json_binary(&BALANCES.load(deps.storage)?).map_err(ContractError::Std)
            }
        },
        _ => Puppeteer::default().query(deps, env, msg),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut<NeutronQuery>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    match msg {
        ExecuteMsg::Delegate {
            items,
            timeout,
            reply_to,
        } => execute_delegate(deps, info, items, timeout, reply_to),
        ExecuteMsg::Undelegate {
            items,
            timeout,
            reply_to,
        } => execute_undelegate(deps, info, items, timeout, reply_to),
        ExecuteMsg::Redelegate {
            validator_from,
            validator_to,
            amount,
            timeout,
            reply_to,
        } => execute_redelegate(
            deps,
            info,
            validator_from,
            validator_to,
            amount,
            timeout,
            reply_to,
        ),
        ExecuteMsg::TokenizeShare {
            validator,
            amount,
            timeout,
            reply_to,
        } => execute_tokenize_share(deps, info, validator, amount, timeout, reply_to),
        ExecuteMsg::RedeemShare {
            validator,
            amount,
            denom,
            timeout,
            reply_to,
        } => execute_redeem_share(deps, info, validator, amount, denom, timeout, reply_to),
        ExecuteMsg::ClaimRewardsAndOptionalyTransfer {
            validators,
            transfer,
            timeout,
            reply_to,
        } => execute_claim_rewards_and_optionaly_transfer(
            deps, info, validators, transfer, timeout, reply_to,
        ),
        ExecuteMsg::RegisterDelegatorDelegationsQuery { validators } => {
            register_delegations_query(deps, validators)
        }
        ExecuteMsg::RegisterBalanceQuery { denom } => register_balance_query(deps, denom),
        ExecuteMsg::IBCTransfer { timeout, reply_to } => {
            execute_ibc_transfer(deps, env, info, timeout, reply_to)
        }
        _ => puppeteer_base.execute(deps, env, info, msg.to_base_enum()),
    }
}

fn execute_ibc_transfer(
    deps: DepsMut<NeutronQuery>,
    env: Env,
    info: MessageInfo,
    timeout: u64,
    reply_to: String,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    let config = puppeteer_base.config.load(deps.storage)?;
    validate_sender(&config, &info.sender)?;
    puppeteer_base.validate_tx_idle_state(deps.as_ref())?;
    // exclude fees, no need to send local denom tokens to remote zone
    let message_funds: Vec<_> = info
        .funds
        .iter()
        .filter(|f| f.denom != LOCAL_DENOM)
        .collect();
    ensure_eq!(
        message_funds.len(),
        1,
        ContractError::InvalidFunds {
            reason: "Only one coin is allowed".to_string()
        }
    );
    let coin = message_funds.get(0).ok_or(ContractError::InvalidFunds {
        reason: "No funds".to_string(),
    })?;
    let ica_address = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let msg = NeutronMsg::IbcTransfer {
        source_port: config.port_id,
        source_channel: config.transfer_channel_id,
        token: (*coin).clone(),
        sender: env.contract.address.to_string(),
        receiver: ica_address.to_string(),
        timeout_height: RequestPacketTimeoutHeight {
            revision_number: None,
            revision_height: None,
        },
        timeout_timestamp: env.block.time.plus_seconds(timeout).nanos(),
        memo: "".to_string(),
        fee: puppeteer_base.ibc_fee.load(deps.storage)?,
    };
    let submsg = puppeteer_base.msg_with_sudo_callback(
        deps,
        msg,
        Transaction::IBCTransfer {
            denom: coin.denom.to_string(),
            amount: coin.amount.into(),
            recipient: ica_address,
        },
        reply_to,
        SUDO_PAYLOAD_REPLY_ID,
    )?;
    Ok(Response::default().add_submessages(vec![submsg]))
}

fn register_delegations_query(
    deps: DepsMut<NeutronQuery>,
    validators: Vec<String>,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    let config = puppeteer_base.config.load(deps.storage)?;
    let delegator = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let msg = SubMsg::reply_on_success(
        new_register_delegator_delegations_query_msg(
            config.connection_id,
            delegator,
            validators,
            config.update_period,
        )?,
        SUDO_KV_DELEGATIONS_REPLY_ID,
    );
    deps.api.debug(&format!(
        "WASMDEBUG: register_delegations_query {msg:?}",
        msg = msg
    ));
    Ok(Response::new().add_submessage(msg))
}

fn register_balance_query(
    deps: DepsMut<NeutronQuery>,
    denom: String,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    let config = puppeteer_base.config.load(deps.storage)?;
    let ica = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let msg = SubMsg::reply_on_success(
        new_register_balance_query_msg(config.connection_id, ica, denom, config.update_period)?,
        SUDO_KV_BALANCE_REPLY_ID,
    );
    deps.api.debug(&format!(
        "WASMDEBUG: register_balance_query {msg:?}",
        msg = msg
    ));
    Ok(Response::new().add_submessage(msg))
}

fn execute_delegate(
    mut deps: DepsMut<NeutronQuery>,
    info: MessageInfo,
    items: Vec<(String, Uint128)>,
    timeout: Option<u64>,
    reply_to: String,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    deps.api.addr_validate(&reply_to)?;
    let config: Config = puppeteer_base.config.load(deps.storage)?;
    validate_sender(&config, &info.sender)?;
    puppeteer_base.validate_tx_idle_state(deps.as_ref())?;
    let delegator = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let any_msgs = items
        .iter()
        .map(|(validator, amount)| MsgDelegate {
            delegator_address: delegator.to_string(),
            validator_address: validator.to_string(),
            amount: Some(Coin {
                denom: config.remote_denom.to_string(),
                amount: amount.to_string(),
            }),
        })
        .map(|msg| prepare_any_msg(msg, "/cosmos.staking.v1beta1.MsgDelegate"))
        .collect::<NeutronResult<Vec<ProtobufAny>>>()?;

    let submsg = compose_submsg(
        deps.branch(),
        config.clone(),
        any_msgs,
        Transaction::Delegate {
            interchain_account_id: ICA_ID.to_string(),
            denom: config.remote_denom,
            items,
        },
        timeout,
        reply_to,
        SUDO_PAYLOAD_REPLY_ID,
    )?;

    Ok(Response::default().add_submessages(vec![submsg]))
}

fn execute_claim_rewards_and_optionaly_transfer(
    mut deps: DepsMut<NeutronQuery>,
    info: MessageInfo,
    validators: Vec<String>,
    transfer: Option<TransferReadyBatchMsg>,
    timeout: Option<u64>,
    reply_to: String,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    deps.api.addr_validate(&reply_to)?;
    let config: Config = puppeteer_base.config.load(deps.storage)?;
    validate_sender(&config, &info.sender)?;
    puppeteer_base.validate_tx_idle_state(deps.as_ref())?;
    let ica = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let mut any_msgs = vec![];
    if let Some(transfer) = transfer.clone() {
        let transfer_msg = MsgSend {
            from_address: ica.to_string(),
            to_address: transfer.recipient,
            amount: vec![Coin {
                amount: transfer.amount.to_string(),
                denom: config.remote_denom.to_string(),
            }],
        };
        any_msgs.push(prepare_any_msg(
            transfer_msg,
            "/cosmos.bank.v1beta1.MsgSend",
        )?);
    }
    for val in validators.clone() {
        let withdraw_msg = MsgWithdrawDelegatorReward {
            delegator_address: ica.to_string(),
            validator_address: val,
        };
        any_msgs.push(prepare_any_msg(
            withdraw_msg,
            "/cosmos.distribution.v1beta1.MsgWithdrawDelegatorReward",
        )?);
    }
    let submsg = compose_submsg(
        deps.branch(),
        config.clone(),
        any_msgs,
        Transaction::ClaimRewardsAndOptionalyTransfer {
            interchain_account_id: ICA_ID.to_string(),
            validators,
            denom: config.remote_denom.to_string(),
            transfer,
        },
        timeout,
        reply_to,
        SUDO_PAYLOAD_REPLY_ID,
    )?;

    Ok(Response::default().add_submessages(vec![submsg]))
}

fn execute_undelegate(
    mut deps: DepsMut<NeutronQuery>,
    info: MessageInfo,
    items: Vec<(String, Uint128)>,
    timeout: Option<u64>,
    reply_to: String,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    deps.api.addr_validate(&reply_to)?;
    let config: Config = puppeteer_base.config.load(deps.storage)?;
    validate_sender(&config, &info.sender)?;
    puppeteer_base.validate_tx_idle_state(deps.as_ref())?;
    let delegator = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let any_msgs = items
        .iter()
        .map(|(validator, amount)| MsgUndelegate {
            delegator_address: delegator.to_string(),
            validator_address: validator.to_string(),
            amount: Some(Coin {
                denom: config.remote_denom.to_string(),
                amount: amount.to_string(),
            }),
        })
        .map(|msg| prepare_any_msg(msg, "/cosmos.staking.v1beta1.MsgUndelegate"))
        .collect::<NeutronResult<Vec<ProtobufAny>>>()?;

    let submsg = compose_submsg(
        deps.branch(),
        config.clone(),
        any_msgs,
        Transaction::Undelegate {
            interchain_account_id: ICA_ID.to_string(),
            denom: config.remote_denom,
            items,
        },
        timeout,
        reply_to,
        SUDO_PAYLOAD_REPLY_ID,
    )?;

    Ok(Response::default().add_submessages(vec![submsg]))
}

fn execute_redelegate(
    mut deps: DepsMut<NeutronQuery>,
    info: MessageInfo,
    validator_from: String,
    validator_to: String,
    amount: Uint128,
    timeout: Option<u64>,
    reply_to: String,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    deps.api.addr_validate(&reply_to)?;
    let config: Config = puppeteer_base.config.load(deps.storage)?;
    validate_sender(&config, &info.sender)?;
    puppeteer_base.validate_tx_idle_state(deps.as_ref())?;
    let delegator = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let redelegate_msg = MsgBeginRedelegate {
        delegator_address: delegator,
        validator_src_address: validator_from.to_string(),
        validator_dst_address: validator_to.to_string(),
        amount: Some(ProtoCoin {
            denom: config.remote_denom.to_string(),
            amount: amount.to_string(),
        }),
    };

    let submsg = compose_submsg(
        deps.branch(),
        config.clone(),
        vec![prepare_any_msg(
            redelegate_msg,
            "/cosmos.staking.v1beta1.MsgBeginRedelegate",
        )?],
        Transaction::Redelegate {
            interchain_account_id: ICA_ID.to_string(),
            validator_from,
            validator_to,
            denom: config.remote_denom,
            amount: amount.into(),
        },
        timeout,
        reply_to,
        SUDO_PAYLOAD_REPLY_ID,
    )?;

    Ok(Response::default().add_submessages(vec![submsg]))
}

fn execute_tokenize_share(
    mut deps: DepsMut<NeutronQuery>,
    info: MessageInfo,
    validator: String,
    amount: Uint128,
    timeout: Option<u64>,
    reply_to: String,
) -> ContractResult<Response<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    deps.api.addr_validate(&reply_to)?;
    let config: Config = puppeteer_base.config.load(deps.storage)?;
    validate_sender(&config, &info.sender)?;
    puppeteer_base.validate_tx_idle_state(deps.as_ref())?;
    let delegator = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let tokenize_msg = MsgTokenizeShares {
        delegator_address: delegator.clone(),
        validator_address: validator.to_string(),
        tokenized_share_owner: delegator,
        amount: Some(ProtoCoin {
            denom: config.remote_denom.to_string(),
            amount: amount.to_string(),
        }),
    };
    let submsg = compose_submsg(
        deps.branch(),
        config.clone(),
        vec![prepare_any_msg(
            tokenize_msg,
            "/cosmos.staking.v1beta1.MsgTokenizeShares",
        )?],
        Transaction::TokenizeShare {
            interchain_account_id: ICA_ID.to_string(),
            validator,
            denom: config.remote_denom,
            amount: amount.into(),
        },
        timeout,
        reply_to,
        SUDO_PAYLOAD_REPLY_ID,
    )?;

    Ok(Response::default().add_submessages(vec![submsg]))
}

fn execute_redeem_share(
    mut deps: DepsMut<NeutronQuery>,
    info: MessageInfo,
    validator: String,
    amount: Uint128,
    denom: String,
    timeout: Option<u64>,
    reply_to: String,
) -> ContractResult<Response<NeutronMsg>> {
    let attrs = vec![
        attr("action", "redeem_share"),
        attr("validator", validator.clone()),
        attr("amount", amount.to_string()),
        attr("denom", denom.clone()),
    ];
    let puppeteer_base = Puppeteer::default();
    deps.api.addr_validate(&reply_to)?;
    puppeteer_base.validate_tx_idle_state(deps.as_ref())?;
    let config: Config = puppeteer_base.config.load(deps.storage)?;
    validate_sender(&config, &info.sender)?;
    let delegator = puppeteer_base.get_ica(&puppeteer_base.state.load(deps.storage)?)?;
    let redeem_msg = MsgRedeemTokensforShares {
        delegator_address: delegator,
        amount: Some(ProtoCoin {
            denom: denom.to_string(),
            amount: amount.to_string(),
        }),
    };
    let submsg = compose_submsg(
        deps.branch(),
        config,
        vec![prepare_any_msg(
            redeem_msg,
            "/cosmos.staking.v1beta1.MsgRedeemTokensForShares",
        )?],
        Transaction::RedeemShare {
            interchain_account_id: ICA_ID.to_string(),
            validator,
            denom,
            amount: amount.into(),
        },
        timeout,
        reply_to,
        SUDO_PAYLOAD_REPLY_ID,
    )?;
    Ok(Response::default()
        .add_submessages(vec![submsg])
        .add_attributes(attrs))
}

fn prepare_any_msg<T: prost::Message>(msg: T, type_url: &str) -> NeutronResult<ProtobufAny> {
    let mut buf = Vec::new();
    buf.reserve(msg.encoded_len());

    if let Err(e) = msg.encode(&mut buf) {
        return Err(NeutronError::Std(StdError::generic_err(format!(
            "Encode error: {e}"
        ))));
    }
    Ok(ProtobufAny {
        type_url: type_url.to_string(),
        value: Binary::from(buf),
    })
}

fn compose_submsg(
    mut deps: DepsMut<NeutronQuery>,
    config: Config,
    any_msgs: Vec<ProtobufAny>,
    transaction: Transaction,
    timeout: Option<u64>,
    reply_to: String,
    reply_id: u64,
) -> NeutronResult<SubMsg<NeutronMsg>> {
    let puppeteer_base = Puppeteer::default();
    let ibc_fee: IbcFee = puppeteer_base.ibc_fee.load(deps.storage)?;
    let connection_id = config.connection_id;
    let cosmos_msg = NeutronMsg::submit_tx(
        connection_id,
        ICA_ID.to_string(),
        any_msgs,
        "".to_string(),
        timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
        ibc_fee,
    );
    let submsg = puppeteer_base.msg_with_sudo_callback(
        deps.branch(),
        cosmos_msg,
        transaction,
        reply_to,
        reply_id,
    )?;
    Ok(submsg)
}

#[entry_point]
pub fn sudo(deps: DepsMut<NeutronQuery>, env: Env, msg: SudoMsg) -> NeutronResult<Response> {
    let puppeteer_base = Puppeteer::default();
    deps.api.debug(&format!(
        "WASMDEBUG: sudo call: {:?} block: {:?}",
        msg, env.block
    ));
    match msg {
        SudoMsg::Response { request, data } => sudo_response(deps, env, request, data),
        SudoMsg::Error { request, details } => sudo_error(deps, env, request, details),
        SudoMsg::Timeout { request } => sudo_timeout(deps, env, request),
        SudoMsg::TxQueryResult {
            query_id,
            height,
            data,
        } => puppeteer_base.sudo_tx_query_result(deps, env, query_id, height, data),
        SudoMsg::KVQueryResult { query_id } => {
            let query_type = puppeteer_base.kv_queries.load(deps.storage, query_id)?;
            match query_type {
                KVQueryType::Balance => {
                    puppeteer_base.sudo_kv_query_result(deps, env, query_id, BALANCES)
                }
                KVQueryType::Delegations => {
                    puppeteer_base.sudo_kv_query_result(deps, env, query_id, DELEGATIONS)
                }
            }
        }
        SudoMsg::OpenAck {
            port_id,
            channel_id,
            counterparty_channel_id,
            counterparty_version,
        } => puppeteer_base.sudo_open_ack(
            deps,
            env,
            port_id,
            channel_id,
            counterparty_channel_id,
            counterparty_version,
        ),
    }
}

fn sudo_response(
    deps: DepsMut<NeutronQuery>,
    _env: Env,
    request: RequestPacket,
    data: Binary,
) -> NeutronResult<Response> {
    deps.api.debug("WASMDEBUG: sudo response");
    let attrs = vec![
        attr("action", "sudo_response"),
        attr("request_id", request.sequence.unwrap_or(0).to_string()),
    ];
    let puppeteer_base = Puppeteer::default();
    let seq_id = request
        .sequence
        .ok_or_else(|| StdError::generic_err("sequence not found"))?;
    let tx_state = puppeteer_base.tx_state.load(deps.storage)?;
    deps.api
        .debug(&format!("WASMDEBUG: tx_state {:?}", tx_state));
    puppeteer_base.validate_tx_waiting_state(deps.as_ref())?;
    deps.api.debug(&format!("WASMDEBUG: state is ok"));
    let reply_to = tx_state
        .reply_to
        .ok_or_else(|| StdError::generic_err("reply_to not found"))?;
    deps.api.debug(&format!(
        "WASMDEBUG: reply_to: {reply_to}",
        reply_to = reply_to
    ));
    let transaction = tx_state
        .transaction
        .ok_or_else(|| StdError::generic_err("transaction not found"))?;
    deps.api.debug(&format!(
        "WASMDEBUG: transaction: {transaction:?}",
        transaction = transaction
    ));
    puppeteer_base.tx_state.save(
        deps.storage,
        &TxState {
            status: TxStateStatus::Idle,
            seq_id: None,
            transaction: None,
            reply_to: None,
        },
    )?;
    let answers = match transaction {
        Transaction::IBCTransfer { .. } => vec![ResponseAnswer::IBCTransfer(MsgIBCTransfer {})],
        _ => {
            let msg_data: TxMsgData = TxMsgData::decode(data.as_slice())?;
            get_answers_from_msg_data(deps.as_ref(), msg_data)?
        }
    };
    deps.api.debug(&format!(
        "WASMDEBUG: json: {request:?}",
        request = to_json_binary(&ReceiverExecuteMsg::PuppeteerHook(
            ResponseHookMsg::Success(ResponseHookSuccessMsg {
                request_id: seq_id,
                request: request.clone(),
                transaction: transaction.clone(),
                answers: answers.clone(),
            },)
        ))?
    ));
    let msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: reply_to.clone(),
        msg: to_json_binary(&ReceiverExecuteMsg::PuppeteerHook(
            ResponseHookMsg::Success(ResponseHookSuccessMsg {
                request_id: seq_id,
                request: request.clone(),
                transaction: transaction.clone(),
                answers,
            }),
        ))?,
        funds: vec![],
    });
    Ok(response("sudo-response", "puppeteer", attrs).add_message(msg))
}

fn get_answers_from_msg_data(
    deps: Deps<NeutronQuery>,
    msg_data: TxMsgData,
) -> NeutronResult<Vec<ResponseAnswer>> {
    let mut answers = vec![];
    #[allow(deprecated)]
    for item in msg_data.data {
        let answer = match item.msg_type.as_str() {
            "/cosmos.staking.v1beta1.MsgDelegate" => {
                let _out: MsgDelegateResponse = decode_message_response(&item.data)?;
                lido_puppeteer_base::msg::ResponseAnswer::DelegateResponse(
                    lido_puppeteer_base::proto::MsgDelegateResponse {},
                )
            }
            "/cosmos.staking.v1beta1.MsgUndelegate" => {
                let out: MsgUndelegateResponse = decode_message_response(&item.data)?;
                lido_puppeteer_base::msg::ResponseAnswer::UndelegateResponse(
                    lido_puppeteer_base::proto::MsgUndelegateResponse {
                        completion_time: out.completion_time.map(|t| t.into()),
                    },
                )
            }
            "/cosmos.staking.v1beta1.MsgTokenizeShares" => {
                let out: MsgTokenizeSharesResponse = decode_message_response(&item.data)?;
                lido_puppeteer_base::msg::ResponseAnswer::TokenizeSharesResponse(
                    lido_puppeteer_base::proto::MsgTokenizeSharesResponse {
                        amount: out.amount.map(convert_coin).transpose()?,
                    },
                )
            }
            "/cosmos.staking.v1beta1.MsgBeginRedelegate" => {
                let out: MsgBeginRedelegateResponse = decode_message_response(&item.data)?;
                lido_puppeteer_base::msg::ResponseAnswer::BeginRedelegateResponse(
                    lido_puppeteer_base::proto::MsgBeginRedelegateResponse {
                        completion_time: out.completion_time.map(|t| t.into()),
                    },
                )
            }
            "/cosmos.staking.v1beta1.MsgRedeemTokensForShares" => {
                let out: MsgRedeemTokensforSharesResponse = decode_message_response(&item.data)?;
                lido_puppeteer_base::msg::ResponseAnswer::RedeemTokensforSharesResponse(
                    lido_puppeteer_base::proto::MsgRedeemTokensforSharesResponse {
                        amount: out.amount.map(convert_coin).transpose()?,
                    },
                )
            }
            _ => {
                deps.api.debug(
                    format!("This type of acknowledgement is not implemented: {item:?}").as_str(),
                );
                lido_puppeteer_base::msg::ResponseAnswer::UnknownResponse {}
            }
        };
        deps.api.debug(&format!(
            "WASMDEBUG: sudo_response: answer: {answer:?}",
            answer = answer
        ));
        answers.push(answer);
    }
    Ok(answers)
}

fn convert_coin(coin: crate::proto::cosmos::base::v1beta1::Coin) -> StdResult<cosmwasm_std::Coin> {
    Ok(cosmwasm_std::Coin {
        denom: coin.denom,
        amount: Uint128::from_str(&coin.amount)?,
    })
}

fn sudo_error(
    deps: DepsMut<NeutronQuery>,
    _env: Env,
    request: RequestPacket,
    details: String,
) -> NeutronResult<Response> {
    let attrs = vec![
        attr("action", "sudo_error"),
        attr("request_id", request.sequence.unwrap_or(0).to_string()),
        attr("details", details.clone()),
    ];
    let puppeteer_base: PuppeteerBase<'_, Config, KVQueryType> = Puppeteer::default();
    deps.api.debug(&format!(
        "WASMDEBUG: sudo_error: request: {request:?} details: {details:?}",
        request = request,
        details = details
    ));
    let tx_state = puppeteer_base.tx_state.load(deps.storage)?;
    puppeteer_base.validate_tx_waiting_state(deps.as_ref())?;

    let seq_id = request
        .sequence
        .ok_or_else(|| StdError::generic_err("sequence not found"))?;

    let msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: tx_state
            .reply_to
            .ok_or_else(|| StdError::generic_err("reply_to not found"))?,
        msg: to_json_binary(&ReceiverExecuteMsg::PuppeteerHook(ResponseHookMsg::Error(
            ResponseHookErrorMsg {
                request_id: seq_id,
                request,
                details,
            },
        )))?,
        funds: vec![],
    });
    puppeteer_base.tx_state.save(
        deps.storage,
        &TxState {
            status: TxStateStatus::Idle,
            seq_id: None,
            transaction: None,
            reply_to: None,
        },
    )?;
    Ok(response("sudo-error", "puppeteer", attrs).add_message(msg))
}

fn sudo_timeout(
    deps: DepsMut<NeutronQuery>,
    _env: Env,
    request: RequestPacket,
) -> NeutronResult<Response> {
    deps.api.debug(&format!(
        "WASMDEBUG: sudo_timeout: request: {request:?}",
        request = request
    ));
    let attrs = vec![
        attr("action", "sudo_timeout"),
        attr("request_id", request.sequence.unwrap_or(0).to_string()),
    ];
    let puppeteer_base = Puppeteer::default();
    let seq_id = request
        .sequence
        .ok_or_else(|| StdError::generic_err("sequence not found"))?;
    let tx_state = puppeteer_base.tx_state.load(deps.storage)?;
    puppeteer_base.validate_tx_waiting_state(deps.as_ref())?;
    puppeteer_base.state.save(
        deps.storage,
        &State {
            ica: None,
            last_processed_height: None,
            ica_state: IcaState::Timeout,
        },
    )?;
    puppeteer_base.tx_state.save(
        deps.storage,
        &TxState {
            status: TxStateStatus::Idle,
            seq_id: None,
            transaction: None,
            reply_to: None,
        },
    )?;
    deps.api.debug(&format!(
        "WASMDEBUG: sudo_timeout: request: {request:?}",
        request = request
    ));
    let msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: tx_state
            .reply_to
            .ok_or_else(|| StdError::generic_err("reply_to not found"))?,
        msg: to_json_binary(&ReceiverExecuteMsg::PuppeteerHook(ResponseHookMsg::Error(
            ResponseHookErrorMsg {
                request_id: seq_id,
                request,
                details: "Timeout".to_string(),
            },
        )))?,
        funds: vec![],
    });
    Ok(response("sudo-timeout", "puppeteer", attrs).add_message(msg))
}

#[entry_point]
pub fn reply(deps: DepsMut, env: Env, msg: Reply) -> StdResult<Response> {
    let puppeteer_base: PuppeteerBase<'_, Config, KVQueryType> = Puppeteer::default();
    match msg.id {
        SUDO_PAYLOAD_REPLY_ID => puppeteer_base.submit_tx_reply(deps, env, msg),
        SUDO_IBC_TRANSFER_REPLY_ID => puppeteer_base.submit_ibc_transfer_reply(deps, env, msg),
        SUDO_KV_BALANCE_REPLY_ID => {
            deps.api
                .debug(&format!("WASMDEBUG: KV_BALANCE_REPLY_ID {:?}", msg));
            puppeteer_base.register_kv_query_reply(deps, env, msg, KVQueryType::Balance)
        }
        SUDO_KV_DELEGATIONS_REPLY_ID => {
            deps.api
                .debug(&format!("WASMDEBUG: DELEGATIONS_REPLY_ID {:?}", msg));
            puppeteer_base.register_kv_query_reply(deps, env, msg, KVQueryType::Delegations)
        }
        _ => Err(StdError::generic_err("Unknown reply id")),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    deps.api.debug("WASMDEBUG: migrate");
    Ok(Response::default())
}

fn validate_sender(config: &Config, sender: &Addr) -> StdResult<()> {
    if config.allowed_senders.contains(sender) {
        Ok(())
    } else {
        Err(StdError::generic_err("Sender is not allowed"))
    }
}
