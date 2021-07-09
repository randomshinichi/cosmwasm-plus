/*
 * This is a set of helpers meant for use with @cosmjs/cli
 * With these you can easily use the cw20 contract without worrying about forming messages and parsing queries.
 *
 * Usage: npx @cosmjs/cli@^0.23 --init https://raw.githubusercontent.com/CosmWasm/cosmwasm-plus/master/contracts/cw20-base/helpers.ts
 *
 * Create a client:
 *   const client = await useOptions(hackatomOptions).setup(password);
 *   await client.getAccount()
 *
 * Get the mnemonic:
 *   await useOptions(hackatomOptions).recoverMnemonic(password)
 *
 * If you want to use this code inside an app, you will need several imports from https://github.com/CosmWasm/cosmjs
 */
import axios from  "axios"
import { SigningCosmWasmClient } from "@cosmjs/cosmwasm-stargate";

const path = require("path");

interface Options {
  readonly httpUrl: string
  readonly networkId: string
  readonly feeToken: string
  readonly gasPrice: GasPrice
  readonly bech32prefix: string
  readonly hdPath: readonly Slip10RawIndex[]
  readonly faucetUrl?: string
  readonly defaultKeyFile: string
  readonly gasLimits: Partial<GasLimits<CosmWasmFeeTable>> // only set the ones you want to override
}

const hackatomOptions: Options = {
  httpUrl: 'https://lcd.heldernet.cosmwasm.com',
  networkId: 'hackatom-wasm',
  gasPrice:  GasPrice.fromString("0.025ucosm"),
  bech32prefix: 'cosmos',
  feeToken: 'ucosm',
  faucetUrl: 'https://faucet.heldernet.cosmwasm.com/credit',
  hdPath: makeCosmoshubPath(0),
  defaultKeyFile: path.join(process.env.HOME, ".heldernet.key"),
  gasLimits: {
    upload: 1500000,
    init: 600000,
    register:800000,
    transfer: 80000,
  },
}

interface Network {
  setup: (password: string, filename?: string) => Promise<SigningCosmWasmClient>
  recoverMnemonic: (password: string, filename?: string) => Promise<string>
}

const useOptions = (options: Options): Network => {

  const loadOrCreateWallet = async (options: Options, filename: string, password: string): Promise<Secp256k1HdWallet> => {
    let encrypted: string;
    try {
      encrypted = fs.readFileSync(filename, 'utf8');
    } catch (err) {
      // generate if no file exists
      const wallet = await Secp256k1HdWallet.generate(12, options.hdPath, options.bech32prefix);
      const encrypted = await wallet.serialize(password);
      fs.writeFileSync(filename, encrypted, 'utf8');
      return wallet;
    }
    // otherwise, decrypt the file (we cannot put deserialize inside try or it will over-write on a bad password)
    const wallet = await Secp256k1HdWallet.deserialize(encrypted, password);
    return wallet;
  };

  const connect = async (
    wallet: Secp256k1HdWallet,
    options: Options
  ): Promise<SigningCosmWasmClient> => {
    const [{ address }] = await wallet.getAccounts();

    const client = new SigningCosmWasmClient(
      options.httpUrl,
      address,
      wallet,
      hackatomOptions.gasPrice,
      hackatomOptions.gasLimits,
    );
    return client;
  };


  const hitFaucet = async (
    faucetUrl: string,
    address: string,
    ticker: string
  ): Promise<void> => {
    await axios.post(faucetUrl, { ticker, address });
  }

  const setup = async (password: string, filename?: string): Promise<SigningCosmWasmClient> => {
    const keyfile = filename || options.defaultKeyFile;
    const wallet = await loadOrCreateWallet(hackatomOptions, keyfile, password);
    const client = await connect(wallet, hackatomOptions);

    // ensure we have some tokens
    if (options.faucetUrl) {
      const account = await client.getAccount();
      if (!account) {
        console.log(`Getting ${options.feeToken} from faucet`);
        await hitFaucet(options.faucetUrl, client.senderAddress, options.feeToken);
      }
    }

    return client;
  }


  const recoverMnemonic = async (password: string, filename?: string): Promise<string> => {
    const keyfile = filename || options.defaultKeyFile;
    const wallet = await loadOrCreateWallet(hackatomOptions, keyfile, password);
    return wallet.mnemonic;
  }

  return {setup, recoverMnemonic};
}

interface Balances {
  readonly address: string
  readonly amount: string  // decimal as string
}

interface MintInfo {
  readonly minter: string
  readonly cap?: string // decimal as string
}

type Expiration = {readonly at_height: number} | {readonly at_time: number} | {readonly never: {}};

interface AllowanceResponse {
  readonly allowance: string;  // integer as string
  readonly expires: Expiration;
}

interface AllowanceInfo {
  readonly allowance: string;  // integer as string
  readonly spender: string; // bech32 address
  readonly expires: Expiration;
}

interface AllAllowancesResponse {
  readonly allowances: readonly AllowanceInfo[];
}

interface AllAccountsResponse {
  // list of bech32 address that have a balance
  readonly accounts: readonly string[];
}


interface CW20Instance {
  readonly contractAddress: string

  // queries
  balance: (address?: string) => Promise<string>
  allowance: (owner: string, spender: string) => Promise<AllowanceResponse>
  allAllowances: (owner: string, startAfter?: string, limit?: number) => Promise<AllAllowancesResponse>
  allAccounts: (startAfter?: string, limit?: number) => Promise<readonly string[]>
  tokenInfo: () => Promise<any>
  minter: () => Promise<any>

  // actions
  mint: (txSigner: string, recipient: string, amount: string) => Promise<string>
  transfer: (txSigner: string, recipient: string, amount: string) => Promise<string>
  burn: (txSigner: string, amount: string) => Promise<string>
  increaseAllowance: (txSigner: string, recipient: string, amount: string) => Promise<string>
  decreaseAllowance: (txSigner: string, recipient: string, amount: string) => Promise<string>
  transferFrom: (txSigner: string, owner: string, recipient: string, amount: string) => Promise<string>
}

interface CW20Contract {
  // upload a code blob and returns a codeId
  upload: (txSigner: string) => Promise<number>

  // instantiates a cw20 contract
  // codeId must come from a previous deploy
  // label is the public name of the contract in listing
  // if you set admin, you can run migrations on this contract (likely client.signerAddress)
  instantiate: (txSigner: string, codeId: number, initMsg: Record<string, unknown>, label: string, admin?: string) => Promise<CW20Instance>

  use: (contractAddress: string) => CW20Instance
}


export const CW20 = (client: SigningCosmWasmClient): CW20Contract => {
  const use = (contractAddress: string): CW20Instance => {
    const balance = async (account: string): Promise<string> => {
      const result = await client.queryContractSmart(contractAddress, {balance: { account }});
      return result.balance;
    };

    const allowance = async (owner: string, spender: string): Promise<AllowanceResponse> => {
      return client.queryContractSmart(contractAddress, {allowance: { owner, spender }});
    };

    const allAllowances = async (owner: string, startAfter?: string, limit?: number): Promise<AllAllowancesResponse> => {
      return client.queryContractSmart(contractAddress, {all_allowances: { owner, start_after: startAfter, limit }});
    };

    const allAccounts = async (startAfter?: string, limit?: number): Promise<readonly string[]> => {
      const accounts: AllAccountsResponse = await client.queryContractSmart(contractAddress, {all_accounts: { start_after: startAfter, limit }});
      return accounts.accounts;
    };

    const tokenInfo = async (): Promise<any> => {
      return client.queryContractSmart(contractAddress, {token_info: { }});
    };

    const minter = async (): Promise<any> => {
      return client.queryContractSmart(contractAddress, {minter: { }});
    };

    // mints tokens, returns transactionHash
    const mint = async (signerAddress: string, recipient: string, amount: string): Promise<string> => {
      const result = await client.execute(signerAddress, contractAddress, {mint: {recipient, amount}});
      return result.transactionHash;
    }

    // transfers tokens, returns transactionHash
    const transfer = async (signerAddress: string, recipient: string, amount: string): Promise<string> => {
      const result = await client.execute(signerAddress, contractAddress, {transfer: {recipient, amount}});
      return result.transactionHash;
    }

    // burns tokens, returns transactionHash
    const burn = async (signerAddress: string, amount: string): Promise<string> => {
      const result = await client.execute(signerAddress, contractAddress, {burn: {amount}});
      return result.transactionHash;
    }

    const increaseAllowance = async (signerAddress: string, spender: string, amount: string): Promise<string> => {
      const result = await client.execute(signerAddress, contractAddress, {increase_allowance: {spender, amount}});
      return result.transactionHash;
    }

    const decreaseAllowance = async (signerAddress: string, spender: string, amount: string): Promise<string> => {
      const result = await client.execute(signerAddress, contractAddress, {decrease_allowance: {spender, amount}});
      return result.transactionHash;
    }

    const transferFrom = async (signerAddress: string, owner: string, recipient: string, amount: string): Promise<string> => {
      const result = await client.execute(signerAddress, contractAddress, {transfer_from: {owner, recipient, amount}});
      return result.transactionHash;
    }

    return {
      contractAddress,
      balance,
      allowance,
      allAllowances,
      allAccounts,
      tokenInfo,
      minter,
      mint,
      transfer,
      burn,
      increaseAllowance,
      decreaseAllowance,
      transferFrom,
    };
  }

  const downloadWasm = async (url: string): Promise<Uint8Array> => {
    const r = await axios.get(url, { responseType: 'arraybuffer' })
    if (r.status !== 200) {
      throw new Error(`Download error: ${r.status}`)
    }
    return r.data
  }

  const upload = async (signerAddress: string): Promise<number> => {
    const meta = {
      source: "https://github.com/CosmWasm/cosmwasm-plus/tree/v0.6.2/contracts/cw20-base",
      builder: "cosmwasm/workspace-optimizer:0.10.7"
    };
    const sourceUrl = "https://github.com/CosmWasm/cosmwasm-plus/releases/download/v0.6.2/cw20_base.wasm";
    const wasm = await downloadWasm(sourceUrl);
    const result = await client.upload(signerAddress, wasm, meta);
    return result.codeId;
  }

  const instantiate = async (signerAddress: string, codeId: number, initMsg: Record<string, unknown>, label: string, admin?: string): Promise<CW20Instance> => {
    const result = await client.instantiate(signerAddress, codeId, initMsg, label, { memo: `Init ${label}`, admin});
    return use(result.contractAddress);
  }

  return { upload, instantiate, use };
}
