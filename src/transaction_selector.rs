/// The transaction selector is responsible for filtering transactions
/// in the plugin framework.
use {log::*, solana_pubkey::Pubkey, std::collections::HashSet};

pub struct TransactionSelector {
    pub mentioned_addresses: HashSet<Vec<u8>>,
    pub select_all_transactions: bool,
    pub select_all_vote_transactions: bool,
}

#[allow(dead_code)]
impl TransactionSelector {
    // Kept as an inherent constructor (the selector is public for pg_backfill).
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Self {
        Self {
            mentioned_addresses: HashSet::default(),
            select_all_transactions: false,
            select_all_vote_transactions: false,
        }
    }

    /// Create a selector based on the mentioned addresses
    /// To select all transactions use ["*"] or ["all"]
    /// To select all vote transactions, use ["all_votes"]
    /// To select transactions mentioning specific addresses use ["<pubkey1>", "<pubkey2>", ...]
    /// Duplicate addresses are ignored; an address that is not a base58 encoded
    /// 32-byte key is an error.
    pub fn new(mentioned_addresses: &[String]) -> Result<Self, String> {
        let select_all_transactions = mentioned_addresses
            .iter()
            .any(|key| key == "*" || key == "all");
        if select_all_transactions {
            info!("Creating TransactionSelector selecting all transactions");
            return Ok(Self {
                mentioned_addresses: HashSet::default(),
                select_all_transactions,
                select_all_vote_transactions: true,
            });
        }
        let select_all_vote_transactions = mentioned_addresses.iter().any(|key| key == "all_votes");
        if select_all_vote_transactions {
            info!("Creating TransactionSelector selecting all vote transactions");
            return Ok(Self {
                mentioned_addresses: HashSet::default(),
                select_all_transactions,
                select_all_vote_transactions: true,
            });
        }

        let mut unique_addresses = HashSet::with_capacity(mentioned_addresses.len());
        for key in mentioned_addresses {
            let address = bs58::decode(key)
                .into_vec()
                .map_err(|err| format!("Invalid transaction_selector mention {key:?}: {err}"))?;
            if address.len() != 32 {
                return Err(format!(
                    "Invalid transaction_selector mention {key:?}: decoded to {} bytes",
                    address.len()
                ));
            }
            if !unique_addresses.insert(address) {
                warn!("Ignoring duplicate transaction_selector mention {key}");
            }
        }
        info!(
            "Creating TransactionSelector from {} unique addresses",
            unique_addresses.len()
        );

        Ok(Self {
            mentioned_addresses: unique_addresses,
            select_all_transactions: false,
            select_all_vote_transactions: false,
        })
    }

    /// Check if a transaction is of interest.
    pub fn is_transaction_selected<'a>(
        &self,
        is_vote: bool,
        mut mentioned_addresses: impl Iterator<Item = &'a Pubkey>,
    ) -> bool {
        if !self.is_enabled() {
            return false;
        }

        if self.select_all_transactions || (self.select_all_vote_transactions && is_vote) {
            return true;
        }
        mentioned_addresses.any(|address| self.mentioned_addresses.contains(address.as_ref()))
    }

    /// Check if any transaction is of interest at all
    pub fn is_enabled(&self) -> bool {
        self.select_all_transactions
            || self.select_all_vote_transactions
            || !self.mentioned_addresses.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn test_select_transaction() {
        let pubkey1 = Pubkey::new_from_array([1; 32]);
        let pubkey2 = Pubkey::new_from_array([2; 32]);

        let selector = TransactionSelector::new(&[pubkey1.to_string()]).unwrap();

        assert!(selector.is_enabled());

        let addresses = [pubkey1];

        assert!(selector.is_transaction_selected(false, Box::new(addresses.iter())));

        let addresses = [pubkey2];
        assert!(!selector.is_transaction_selected(false, Box::new(addresses.iter())));

        let addresses = [pubkey1, pubkey2];
        assert!(selector.is_transaction_selected(false, Box::new(addresses.iter())));
    }

    #[test]
    fn test_select_all_transaction_using_wildcard() {
        let pubkey1 = Pubkey::new_from_array([1; 32]);
        let pubkey2 = Pubkey::new_from_array([2; 32]);

        let selector = TransactionSelector::new(&["*".to_string()]).unwrap();

        assert!(selector.is_enabled());

        let addresses = [pubkey1];

        assert!(selector.is_transaction_selected(false, Box::new(addresses.iter())));

        let addresses = [pubkey2];
        assert!(selector.is_transaction_selected(false, Box::new(addresses.iter())));

        let addresses = [pubkey1, pubkey2];
        assert!(selector.is_transaction_selected(false, Box::new(addresses.iter())));
    }

    #[test]
    fn test_select_all_transaction_all() {
        let pubkey1 = Pubkey::new_from_array([1; 32]);
        let pubkey2 = Pubkey::new_from_array([2; 32]);

        let selector = TransactionSelector::new(&["all".to_string()]).unwrap();

        assert!(selector.is_enabled());

        let addresses = [pubkey1];

        assert!(selector.is_transaction_selected(false, Box::new(addresses.iter())));

        let addresses = [pubkey2];
        assert!(selector.is_transaction_selected(false, Box::new(addresses.iter())));

        let addresses = [pubkey1, pubkey2];
        assert!(selector.is_transaction_selected(false, Box::new(addresses.iter())));
    }

    #[test]
    fn test_select_all_vote_transaction() {
        let pubkey1 = Pubkey::new_from_array([1; 32]);
        let pubkey2 = Pubkey::new_from_array([2; 32]);

        let selector = TransactionSelector::new(&["all_votes".to_string()]).unwrap();

        assert!(selector.is_enabled());

        let addresses = [pubkey1];

        assert!(!selector.is_transaction_selected(false, Box::new(addresses.iter())));

        let addresses = [pubkey2];
        assert!(selector.is_transaction_selected(true, Box::new(addresses.iter())));

        let addresses = [pubkey1, pubkey2];
        assert!(selector.is_transaction_selected(true, Box::new(addresses.iter())));
    }

    #[test]
    fn test_select_no_transaction() {
        let pubkey1 = Pubkey::new_from_array([1; 32]);
        let pubkey2 = Pubkey::new_from_array([2; 32]);

        let selector = TransactionSelector::new(&[]).unwrap();

        assert!(!selector.is_enabled());

        let addresses = [pubkey1];

        assert!(!selector.is_transaction_selected(false, Box::new(addresses.iter())));

        let addresses = [pubkey2];
        assert!(!selector.is_transaction_selected(true, Box::new(addresses.iter())));

        let addresses = [pubkey1, pubkey2];
        assert!(!selector.is_transaction_selected(true, Box::new(addresses.iter())));
    }

    #[test]
    fn test_duplicate_and_invalid_mentions() {
        let pubkey1 = Pubkey::new_from_array([1; 32]);
        let selector =
            TransactionSelector::new(&[pubkey1.to_string(), pubkey1.to_string()]).unwrap();
        assert_eq!(selector.mentioned_addresses.len(), 1);
        assert!(selector.is_transaction_selected(false, [pubkey1].iter()));

        assert!(TransactionSelector::new(&["not-base58-0OIl".to_string()]).is_err());
        assert!(TransactionSelector::new(&["abc".to_string()]).is_err());
    }
}
