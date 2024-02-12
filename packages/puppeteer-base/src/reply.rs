use cosmwasm_std::{attr, DepsMut, Env, Reply, Response, StdError, StdResult};
use lido_helpers::answer::response;
use neutron_sdk::bindings::msg::{
    MsgIbcTransferResponse, MsgRegisterInterchainQueryResponse, MsgSubmitTxResponse,
};
use serde::{de::DeserializeOwned, Serialize};

use crate::state::{BaseConfig, PuppeteerBase, TxState, TxStateStatus};

impl<'a, T, U> PuppeteerBase<'a, T, U>
where
    T: BaseConfig + Serialize + DeserializeOwned + Clone,
    U: Serialize + DeserializeOwned + Clone,
{
    pub fn register_kv_query_reply(
        &self,
        deps: DepsMut,
        _env: Env,
        msg: Reply,
        query_type: U,
    ) -> StdResult<Response> {
        let resp: MsgRegisterInterchainQueryResponse = serde_json_wasm::from_slice(
            msg.result
                .into_result()
                .map_err(StdError::generic_err)?
                .data
                .ok_or_else(|| StdError::generic_err("no result"))?
                .as_slice(),
        )
        .map_err(|e| StdError::generic_err(format!("failed to parse response: {e:?}")))?;
        deps.api
            .debug(format!("WASMDEBUG: prepare_sudo_payload received; resp: {resp:?}").as_str());
        let atts = vec![attr("query_id", resp.id.to_string())];
        self.kv_queries.save(deps.storage, resp.id, &query_type)?;
        Ok(response(
            "sudo-kv-query-payload-received",
            "puppeteer-base",
            atts,
        ))
    }

    pub fn submit_tx_reply(&self, deps: DepsMut, _env: Env, msg: Reply) -> StdResult<Response> {
        let resp: MsgSubmitTxResponse = serde_json_wasm::from_slice(
            msg.result
                .into_result()
                .map_err(StdError::generic_err)?
                .data
                .ok_or_else(|| StdError::generic_err("no result"))?
                .as_slice(),
        )
        .map_err(|e| StdError::generic_err(format!("failed to parse response: {e:?}")))?;
        deps.api
            .debug(format!("WASMDEBUG: prepare_sudo_payload received; resp: {resp:?}").as_str());
        let seq_id = resp.sequence_id;
        let channel_id = resp.channel;
        let mut self_tx_state: TxState = self.tx_state.load(deps.storage)?;
        self_tx_state.seq_id = Some(seq_id);
        self_tx_state.status = TxStateStatus::WaitingForAck;
        self.tx_state.save(deps.storage, &self_tx_state)?;
        let atts = vec![
            attr("channel_id", channel_id.to_string()),
            attr("seq_id", seq_id.to_string()),
        ];
        Ok(response("sudo-tx-payload-received", "puppeteer-base", atts))
    }

    pub fn submit_ibc_transfer_reply(
        &self,
        deps: DepsMut,
        _env: Env,
        msg: Reply,
    ) -> StdResult<Response> {
        let resp: MsgIbcTransferResponse = serde_json_wasm::from_slice(
            msg.result
                .into_result()
                .map_err(StdError::generic_err)?
                .data
                .ok_or_else(|| StdError::generic_err("no result"))?
                .as_slice(),
        )
        .map_err(|e| StdError::generic_err(format!("failed to parse response: {e:?}")))?;
        deps.api
            .debug(format!("WASMDEBUG: prepare_sudo_payload received; resp: {resp:?}").as_str());
        let seq_id = resp.sequence_id;
        let channel_id = resp.channel;
        let mut self_tx_state: TxState = self.tx_state.load(deps.storage)?;
        self_tx_state.seq_id = Some(seq_id);
        self_tx_state.status = TxStateStatus::WaitingForAck;
        self.tx_state.save(deps.storage, &self_tx_state)?;
        let atts = vec![
            attr("channel_id", channel_id.to_string()),
            attr("seq_id", seq_id.to_string()),
        ];
        Ok(response(
            "sudo-ibc-transfer-payload-received",
            "puppeteer-base",
            atts,
        ))
    }
}
