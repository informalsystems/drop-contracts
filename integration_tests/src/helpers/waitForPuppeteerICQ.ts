import { waitFor } from './waitFor';
import { DropCore, DropPuppeteer } from 'drop-ts-client';
import { ResponseHookSuccessMsg } from 'drop-ts-client/lib/src/contractLib/dropCore';
import { SigningCosmWasmClient } from '@cosmjs/cosmwasm-stargate';

const DropCoreClass = DropCore.Client;
const DropPuppeteerClass = DropPuppeteer.Client;

export const waitForPuppeteerICQ = async (
  client: SigningCosmWasmClient,
  coreContractClient?: InstanceType<typeof DropCoreClass>,
  puppeteerContractClient?: InstanceType<typeof DropPuppeteerClass>,
): Promise<void> => {
  const puppeteerResponse = (
    await coreContractClient.queryLastPuppeteerResponse()
  ).response as {
    success: ResponseHookSuccessMsg;
  };

  const block = await client.getBlock();

  let controlHeight = block.header.height;

  if (puppeteerResponse && puppeteerResponse.success) {
    controlHeight = puppeteerResponse.success.local_height;
  }

  controlHeight++;

  const waitForBalances = waitFor(async () => {
    const [, lastBalanceHeight] = (await puppeteerContractClient.queryExtension(
      {
        msg: {
          balances: {},
        },
      },
    )) as any;
    return lastBalanceHeight > controlHeight;
  }, 50_000);

  const waitForDelegations = waitFor(async () => {
    const [, lastDelegationsHeight] =
      (await puppeteerContractClient.queryExtension({
        msg: {
          delegations: {},
        },
      })) as any;
    return lastDelegationsHeight > controlHeight;
  }, 50_000);

  await Promise.all([waitForBalances, waitForDelegations]);
};
