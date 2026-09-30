import { Address, Contract, NativeToScval, ScInt, ScVal, xdr } from '@stellar-facebood/contract';

export interface TokenConfig {
  name: string;
  symbol: string;
  decimals: number;
  initial_supply: bigint;
}

export interface DeployTokenEvent {
  token: string;
  admin: string;
  config: TokenConfig;
}

export interface FactoryClientExt {
  deploy_token(
    admin: string,
    config: TokenConfig,
  ): Promise<string>;
  get_tokens(): Promise<string[]>;
}

export class FactoryClient extends Contract implements FactoryClientExt {
  constructor(contractId: string, options: ContractConfig) {
    super(contractId, options);
  }

  async deploy_token(admin: string, config: TokenConfig): Promise<string> {
    const adminAddress = new Address(admin);
    const configScval = xdr.ScVal.convertToScval({
      name: config.name,
      symbol: config.symbol,
      decimals: config.decimals,
      initial_supply: config.initial_supply,
    });
    const result = await this.call(
      'deploy_token',
      xdr.ScVal.convertToScval(adminAddress),
      configScval,
    );
    return xdr.ScVal.toString(result);
  }

  async get_tokens(): Promise<string[]> {
    const result = await this.call('get_tokens');
    const tokens = xdr.ScVal.toNative(result) as string[];
    return tokens;
  }
}

export const FactoryClientContract = FactoryClient;
