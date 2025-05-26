use anyhow::{anyhow, Result};
use anchor_client::solana_client::nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient};
use anchor_client::solana_sdk::pubkey::Pubkey;
use anchor_client::solana_sdk::signature::{Keypair, Signer};
use anchor_client::solana_sdk::transaction::Transaction;
use anchor_lang::prelude::Clock;
use anchor_lang::InstructionData;
use commons::{extensions::*, pda::*, quote::quote_exact_in, *};
use dlmm_interface::{
    instructions::{swap2_ix, Swap2IxArgs, Swap2Keys},
    BinArrayAccount, BinArrayBitmapExtensionAccount, LbPairAccount, RemainingAccountsInfo,
};
use solana_sdk::{account::Account, instruction::AccountMeta};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
struct PoolState {
    lb_pair: Option<LbPair>,
    bin_arrays: HashMap<Pubkey, BinArray>,
    bitmap_extension: Option<BinArrayBitmapExtension>,
}

impl PoolState {
    fn update_account(&mut self, pubkey: &Pubkey, data: &[u8]) -> Result<()> {
        if let Ok(account) = LbPairAccount::deserialize(data) {
            self.lb_pair = Some(account.0);
            return Ok(());
        }
        if let Ok(account) = BinArrayAccount::deserialize(data) {
            self.bin_arrays.insert(*pubkey, account.0);
            return Ok(());
        }
        if let Ok(account) = BinArrayBitmapExtensionAccount::deserialize(data) {
            self.bitmap_extension = Some(account.0);
            return Ok(());
        }
        Err(anyhow!("unknown account"))
    }
}

struct SwapQuoteService {
    rpc_http: String,
    rpc_ws: String,
    pair: Pubkey,
    rpc_client: RpcClient,
    state: Arc<Mutex<PoolState>>,
}

impl SwapQuoteService {
    pub fn new(rpc_http: String, rpc_ws: String, pair: Pubkey) -> Self {
        Self {
            rpc_client: RpcClient::new(rpc_http.clone()),
            rpc_http,
            rpc_ws,
            pair,
            state: Arc::new(Mutex::new(PoolState::default())),
        }
    }

    async fn initialize_state(&self) -> Result<()> {
        // Fetch lb pair account
        let acc = self.rpc_client.get_account(&self.pair).await?;
        self.state.lock().unwrap().update_account(&self.pair, &acc.data)?;

        // fetch bitmap extension if exists
        let (bitmap_ext, _) = derive_bin_array_bitmap_extension(self.pair);
        if let Ok(acc) = self.rpc_client.get_account(&bitmap_ext).await {
            self.state.lock().unwrap().update_account(&bitmap_ext, &acc.data)?;
        }

        // fetch active bin array
        if let Some(lb_pair) = &self.state.lock().unwrap().lb_pair {
            let active_idx = BinArray::bin_id_to_bin_array_index(lb_pair.active_id)?;
            for idx in active_idx - 1..=active_idx + 1 {
                let (array_pk, _) = derive_bin_array_pda(self.pair, idx.into());
                if let Ok(acc) = self.rpc_client.get_account(&array_pk).await {
                    self.state.lock().unwrap().update_account(&array_pk, &acc.data)?;
                }
            }
        }
        Ok(())
    }

    async fn subscribe_account(&self, pubkey: Pubkey) -> Result<()> {
        let ws_url = self.rpc_ws.clone();
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            if let Ok((client, mut recv)) = PubsubClient::account_subscribe(&ws_url, &pubkey, None).await {
                while let Some(update) = recv.recv().await {
                    let mut guard = state.lock().unwrap();
                    if let Err(e) = guard.update_account(&pubkey, &update.value.data) {
                        eprintln!("failed to update {}: {}", pubkey, e);
                    }
                }
                drop(client);
            }
        });
        Ok(())
    }

    async fn subscribe(&self) -> Result<()> {
        let mut pubs = vec![self.pair];
        let (bitmap_ext, _) = derive_bin_array_bitmap_extension(self.pair);
        pubs.push(bitmap_ext);

        if let Some(lb_pair) = &self.state.lock().unwrap().lb_pair {
            let idx = BinArray::bin_id_to_bin_array_index(lb_pair.active_id)?;
            for i in idx - 1..=idx + 1 {
                pubs.push(derive_bin_array_pda(self.pair, i.into()).0);
            }
        }

        for pk in pubs {
            self.subscribe_account(pk).await?;
        }
        Ok(())
    }

    pub async fn start(&self) -> Result<()> {
        self.initialize_state().await?;
        self.subscribe().await?;
        Ok(())
    }

    pub fn optimal_quote(&self, amount_in: u64, swap_for_y: bool, clock: &Clock, mint_x: &Account, mint_y: &Account) -> Result<u64> {
        let guard = self.state.lock().unwrap();
        let lb_pair = guard.lb_pair.as_ref().ok_or_else(|| anyhow!("pair not initialized"))?;
        let active_bin_key = BinArray::bin_id_to_bin_array_key(self.pair, lb_pair.active_id)?;
        if let Some(active_array) = guard.bin_arrays.get(&active_bin_key) {
            let mut bin = active_array.clone();
            let mut active_bin = bin.get_bin(lb_pair.active_id)?.clone();
            let price = active_bin.get_or_store_bin_price(lb_pair.active_id, lb_pair.bin_step)?;
            let max_in = active_bin.get_max_amount_in(price, swap_for_y)?;
            if amount_in <= max_in {
                let fee = lb_pair.compute_fee_from_amount(amount_in)?;
                let net = amount_in.checked_sub(fee).unwrap();
                let out = Bin::get_amount_out(net, price, swap_for_y)?;
                return Ok(out);
            }
        }
        let quote = quote_exact_in(
            self.pair,
            lb_pair,
            amount_in,
            swap_for_y,
            guard.bin_arrays.clone(),
            guard.bitmap_extension.as_ref(),
            clock,
            mint_x,
            mint_y,
        )?;
        Ok(quote.amount_out)
    }

    pub async fn build_swap_transaction(
        &self,
        user: &Keypair,
        amount_in: u64,
        swap_for_y: bool,
        slippage_bps: u64,
        clock: &Clock,
        mint_x: &Account,
        mint_y: &Account,
    ) -> Result<String> {
        let quote = self.optimal_quote(amount_in, swap_for_y, clock, mint_x, mint_y)?;
        let min_out = quote
            .checked_mul(10_000u64 - slippage_bps)
            .unwrap()
            .checked_div(10_000)
            .unwrap();

        let guard = self.state.lock().unwrap();
        let lb_pair = guard.lb_pair.as_ref().ok_or_else(|| anyhow!("pair not initialized"))?;
        let [token_x_program, token_y_program] = lb_pair.get_token_programs()?;
        let (event_authority, _) = derive_event_authority_pda();
        let (bitmap_ext, _) = derive_bin_array_bitmap_extension(self.pair);
        let mut remaining_accounts: Vec<AccountMeta> = get_bin_array_pubkeys_for_swap(
            self.pair,
            lb_pair,
            guard.bitmap_extension.as_ref(),
            swap_for_y,
            3,
        )?
        .into_iter()
        .map(|k| AccountMeta::new(k, false))
        .collect();

        let reserve_x = lb_pair.reserve_x;
        let reserve_y = lb_pair.reserve_y;
        let (user_token_in, user_token_out) = if swap_for_y {
            (
                get_associated_token_address_with_program_id(&user.pubkey(), &lb_pair.token_x_mint, &token_x_program),
                get_associated_token_address_with_program_id(&user.pubkey(), &lb_pair.token_y_mint, &token_y_program),
            )
        } else {
            (
                get_associated_token_address_with_program_id(&user.pubkey(), &lb_pair.token_y_mint, &token_y_program),
                get_associated_token_address_with_program_id(&user.pubkey(), &lb_pair.token_x_mint, &token_x_program),
            )
        };

        let keys: [AccountMeta; dlmm_interface::SWAP2_IX_ACCOUNTS_LEN] = Swap2Keys {
            lb_pair: self.pair,
            bin_array_bitmap_extension: bitmap_ext,
            reserve_x,
            reserve_y,
            user_token_in,
            user_token_out,
            token_x_mint: lb_pair.token_x_mint,
            token_y_mint: lb_pair.token_y_mint,
            oracle: lb_pair.oracle,
            host_fee_in: dlmm_interface::ID,
            user: user.pubkey(),
            token_x_program,
            token_y_program,
            memo_program: spl_memo::ID,
            event_authority,
            program: dlmm_interface::ID,
        }
        .into();

        let args = Swap2IxArgs {
            amount_in,
            min_amount_out: min_out,
            remaining_accounts_info: RemainingAccountsInfo { slices: vec![] },
        };
        let ix = swap2_ix(Swap2Keys::from(keys), args)?;
        let mut account_metas = keys.to_vec();
        account_metas.append(&mut remaining_accounts);

        let instruction = solana_sdk::instruction::Instruction {
            program_id: dlmm_interface::ID,
            accounts: account_metas,
            data: ix.data,
        };

        let recent = self.rpc_client.get_latest_blockhash().await?;
        let mut tx = Transaction::new_with_payer(&[instruction], Some(&user.pubkey()));
        tx.sign(&[user], recent);
        let data = bincode::serialize(&tx)?;
        Ok(base64::encode(data))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_http = "https://api.mainnet-beta.solana.com".to_string();
    let rpc_ws = "wss://api.mainnet-beta.solana.com/".to_string();
    let pair = Pubkey::new_unique();
    let service = SwapQuoteService::new(rpc_http, rpc_ws, pair);
    service.start().await?;
    Ok(())
}
