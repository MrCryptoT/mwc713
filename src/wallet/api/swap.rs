use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::{ Mutex };
use failure::Error;
use blake2_rfc::blake2b::blake2b;

use broker::types::{ ContextHolderType };
use broker::MWCMQPublisher;
use common::config::Wallet713Config;

use grinswap::{ Context, Swap };
use grinswap::swap::types::{ RoleContext, SecondarySellerContext, SellerContext, BtcSellerContext };
use grinswap::swap::message::{ Message };
use grin_core::core::{ Transaction, TxKernel };
use grin_p2p::types::PeerInfoDisplay;
use grin_util::secp::pedersen::{ Commitment, RangeProof };
use grin_util::secp::key::SecretKey;
use grin_wallet_libwallet::{ NodeClient, HeaderInfo, WalletBackend };
use grin_keychain::{ExtKeychain, Keychain, Identifier, SwitchCommitmentType };

const GRIN_UNIT: u64 = 1_000_000_000;

pub struct ContextHolder {
    pub context: Context,
    pub stored: bool,
    pub swap: Swap,
}

impl ContextHolderType for ContextHolder {
    fn get_context(&mut self) -> Option<&Context> {
        if !self.stored {
            return None;
        } else {
            return Some(&mut self.context);
        }
    }

    fn get_objs(&mut self) -> Option<(&Context, &mut Swap)> {
        if !self.stored {
            return None;
        } else {
            return Some((&mut self.context, &mut self.swap));
        }
    }

    fn set_context(&mut self, ctx: Context) {
        self.context = ctx;
        self.stored = true;
    }

    fn set_swap(&mut self, swap: Swap) {
        self.swap = swap;
    }

    fn get_swap(&mut self) -> Option<&mut Swap> {
        if !self.stored {
            return None;
        } else {
            return Some(&mut self.swap);
        }
    }
}

#[derive(Debug, Clone)]
struct TestNodeClientState {
    pub height: u64,
    pub total_difficulty: u64,
    pub last_block_pushed: String,
    pub pending: Vec<Transaction>,
    pub outputs: HashMap<Commitment, u64>,
    pub kernels: HashMap<Commitment, (TxKernel, u64)>,
}

#[derive(Debug, Clone)]
struct TestNodeClient {
    pub state: Arc<Mutex<TestNodeClientState>>,
}

impl NodeClient for TestNodeClient {
    fn get_connected_peer_info(&self) -> Result<Vec<PeerInfoDisplay>, grin_wallet_libwallet::Error> {
        unimplemented!()
    }

    fn node_url(&self) -> &str {
        unimplemented!()
    }

    fn set_node_url(&mut self, _node_url: &str) {
        unimplemented!()
    }

    fn node_api_secret(&self) -> Option<String> {
        unimplemented!()
    }

    fn set_node_api_secret(&mut self, _node_api_secret: Option<String>) {
        unimplemented!()
    }

    fn get_header_info(&self, height: u64) -> Result<HeaderInfo, grin_wallet_libwallet::Error> {
        unimplemented!()
    }

    fn post_tx(&self, tx: &grin_wallet_libwallet::TxWrapper, _fluff: bool) -> Result<(), grin_wallet_libwallet::Error> {
        unimplemented!()

        // Implement it here!!!
    }

    fn get_version_info(&mut self) -> Option<grin_wallet_libwallet::NodeVersionInfo> {
        unimplemented!()
    }

    fn get_chain_tip(&self) -> Result<(u64, String, u64), grin_wallet_libwallet::Error> {
        Ok((self.state.lock().height, self.state.lock().last_block_pushed.clone(), self.state.lock().total_difficulty))
    }

    fn get_outputs_from_node(&self, wallet_outputs: Vec<Commitment>,) ->
        Result<HashMap<Commitment, (String, u64, u64)>, grin_wallet_libwallet::Error> {
        unimplemented!()

        // Implement it here!!!
    }

    fn get_outputs_by_pmmr_index(&self, _start_height: u64, _end_height: Option<u64>, _max_outputs: u64,)
        -> Result<(u64, u64, Vec<(Commitment, RangeProof, bool, u64, u64)>), grin_wallet_libwallet::Error> {
        unimplemented!()
    }

    fn height_range_to_pmmr_indices(&self, start_height: u64, end_height: Option<u64>,)
        -> Result<(u64, u64), grin_wallet_libwallet::Error> {
        unimplemented!()
    }

    fn get_kernel(&mut self, excess: &Commitment, _min_height: Option<u64>, _max_height: Option<u64>,)
        -> Result<Option<(TxKernel, u64, u64)>, grin_wallet_libwallet::Error> {
        unimplemented!()

        // Implement it here!!!
    }
}

fn _keychain(idx: u8) -> ExtKeychain {
		let seed_sell: String = format!("fixed0rng0for0testing0purposes0{}", idx % 10);
		let seed_sell = blake2b(32, &[], seed_sell.as_bytes());
		ExtKeychain::from_seed(seed_sell.as_bytes(), false).unwrap()
}

fn key_id(d1: u32, d2: u32) -> Identifier {
		ExtKeychain::derive_key_id(2, d1, d2, 0, 0)
}

fn context_sell(kc: &ExtKeychain) -> Context {
		Context {
			multisig_key: key_id(0, 0),
			multisig_nonce: key(kc, 1, 0),
			lock_nonce: key(kc, 1, 1),
			refund_nonce: key(kc, 1, 2),
			redeem_nonce: key(kc, 1, 3),
			role_context: RoleContext::Seller(SellerContext {
				inputs: vec![
					(key_id(0, 1), 60 * GRIN_UNIT),
					(key_id(0, 2), 60 * GRIN_UNIT),
				],
				change_output: key_id(0, 3),
				refund_output: key_id(0, 4),
				secondary_context: SecondarySellerContext::Btc(BtcSellerContext {
					cosign: key_id(0, 5),
				}),
			}),
		}
}

fn key(kc: &ExtKeychain, d1: u32, d2: u32) -> SecretKey {
		kc.derive_key(0, &key_id(d1, d2), &SwitchCommitmentType::None)
			.unwrap()
}

pub struct SwapProcessor {
}

impl SwapProcessor {

    pub fn process_swap_message<'a, T: ?Sized, C, K>(
                wallet: &mut T,
                from: &dyn crate::contacts::types::Address,
                message: &mut Message,
                config: Option<Wallet713Config>
    ) -> Result<(), Error>
        where
            T: WalletBackend<'a, C, K>,
            C: NodeClient + 'a,
            K: grinswap::Keychain + 'a,
    {

        //println!("Processing swap message: {:?}", message);

        /*let _res = match &message.inner {
            Update::AcceptOffer(_u) => self.process_accept_offer(wallet, from, message, config, publisher),
            Update::Offer(_u) => self.process_offer(wallet, from, message, config, publisher),
            Update::InitRedeem(_u) => self.process_init_redeem(wallet, from, message, config, publisher),
            Update::Redeem(_u) => self.process_redeem(wallet, from, message, config, publisher),
            _ => Err(ErrorKind::Node.into()),
        }?;*/

        Ok(())
    }

    pub fn make_sell_mwc<'a, T: ?Sized, C, K>(&self,
                                        _wallet: &mut T,
                                        _rate: f64,
                                        _qty: u64,
                                        keychain_mask: Option<&SecretKey>,
    ) -> Result<(), Error>
    where
        T: WalletBackend<'a, C, K>,
        C: NodeClient + 'a,
        K: grinswap::Keychain + 'a,
        {
            //let keychain = _wallet.keychain(keychain_mask);
            let kc_sell = _keychain(1);
            //let mut api_sell = BtcSwapApi::new(Some(keychain.clone()), client.clone(), btc_node_client);
            Ok(())
        }

    pub fn swap<'a, T: ?Sized, C, K>(
          wallet: &mut T,
          pair: &str,
          is_make: bool,
          is_buy: bool,
          rate: f64,
          qty: u64,
          address: Option<&str>,
          publisher: &mut MWCMQPublisher,
          btc_redeem: Option<&str>,
    ) -> Result<(), Error>
    where
        T: WalletBackend<'a, C, K>,
        C: NodeClient + 'a,
        K: Keychain + 'a,
        {
            println!("Starting the swap!");

            /*
            let _res = if is_make && is_buy {
                self.make_buy_mwc(wallet, rate, qty)
            } else if is_make && !is_buy {
                self.make_sell_mwc(wallet, rate, qty)
            }
            */
            Ok(())
        }
}

